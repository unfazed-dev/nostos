use super::session::SubscribeRequest;
use nostos_application::{ActiveRuleset, RuleDecision};
use nostos_domain::{ColumnValue, Predicate, PredicateExpr, Principal, ReplicationEvent, SyncMode};

/// Why a subscribe was refused. Rendered into the close reason via `Display`
/// (both `run_session` fatal paths and `register_subscribe`'s
/// `SubscribeReject::Rejected` wrap the rendered string — the SDKs surface it
/// verbatim to app developers, ADR-0031).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SubscribeRejection {
    /// The table is unlisted, or listed with `sync = false` (`toggles` mode).
    /// Fail-closed (ADR-0031 Global Constraint 10): an unlisted table is never
    /// treated as "everything", only ever as "nothing".
    NotSynced { table: String, mode: SyncMode },
    /// The connecting principal lacks a claim the rules' scope for this table
    /// references (e.g. `org_id = claims.org_id` with no `org_id` claim).
    MissingClaim { table: String, claim: String },
    /// `where_sql` failed to compile (ADR-0012) — folded into this enum so
    /// every subscribe refusal shares one path.
    InvalidWhereSql(String),
}

impl std::fmt::Display for SubscribeRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotSynced { table, mode } => write!(
                f,
                "table `{table}` is not synced by the active rules (sync_mode={})",
                mode.as_str()
            ),
            Self::MissingClaim { table, claim } => write!(
                f,
                "missing claim `{claim}` required by the rules for table `{table}`"
            ),
            Self::InvalidWhereSql(reason) => write!(f, "invalid where_sql: {reason}"),
        }
    }
}

/// Build the server-enforced predicate from the client's subscribe + principal.
///
/// Composition order (ADR-0031), all three ANDed, none skippable:
/// **rules scope** (this table's compiled scope from the active ruleset — deny
/// closes the socket before anything else runs) **AND** **tenant scope**
/// (ADR-0011) **AND** **client filters + `where_sql`** (ADR-0012).
///
/// The rules decision runs FIRST and fail-closed: an unlisted/toggled-off
/// table or a missing scope claim rejects the subscribe outright, never
/// falling back to match-all. `ActiveRuleset::all_mode()` always allows
/// (`RuleDecision::Allow(PredicateExpr::any())`) — but tenant scoping below
/// still applies unconditionally in every mode (ADR-0031 Global Constraint
/// 11: `all` mode disables rules, not tenancy).
///
/// The client's filters are always intersected with the tenant filter when a
/// tenant column is configured AND the principal is authenticated — the client
/// cannot widen scope past its own tenant. A client that requests a *different*
/// tenant's value silently gets its own: the server **drops** any client filter
/// on the tenant column and injects the principal's real tenant value (so the
/// predicate is never the impossible `org=X AND org=Y`). Anonymous principals
/// get no injection (single-tenant dev mode).
///
/// The optional `where_sql` (ADR-0012 safe-SQL-subset compiler) is compiled and
/// ANDed in **before** the tenant clause — so the server-injected tenant scoping
/// wraps the client expression and a `where_sql` can never shed it or the rules
/// scope seeded below. A parse failure is `Err(InvalidWhereSql)`; the caller
/// closes the socket with that reason before any event flows.
///
/// The IF-to-enforce-tenant decision is [`Principal::tenant_scope`] — the same
/// seam the write path (`dispatch_write`, ADR-0018) calls, so the read and
/// write enforcement conditions cannot drift apart.
pub(super) fn build_predicate(
    subscribe: &SubscribeRequest,
    principal: &Principal,
    tenant_column: Option<&str>,
    ruleset: &ActiveRuleset,
) -> Result<Predicate, SubscribeRejection> {
    // Rules scope FIRST, fail-closed (ADR-0031). No filter/tenant work happens
    // until the rules allow this table for this principal.
    let rules_expr = match ruleset.decide(&subscribe.table, principal) {
        RuleDecision::Allow(expr) => expr,
        RuleDecision::DeniedTable => {
            return Err(SubscribeRejection::NotSynced {
                table: subscribe.table.clone(),
                mode: ruleset.mode(),
            });
        }
        RuleDecision::DeniedClaim(claim) => {
            return Err(SubscribeRejection::MissingClaim {
                table: subscribe.table.clone(),
                claim,
            });
        }
    };

    let scope = principal.tenant_scope(tenant_column);

    // Seed the predicate from the rules scope (replaces the historical
    // match-all start), then fold in the client's own filters — EXCLUDING any
    // on the tenant column, which the server overrides with the principal's
    // real value (never client-attested). The `and_eq` combinator collapses an
    // `Any` root down to a bare `Eq` leaf (all-mode, no rules scope) and
    // otherwise ANDs onto whatever the rules already seeded — same combinator,
    // no special-casing needed here.
    let mut p = Predicate {
        table: subscribe.table.clone(),
        expr: rules_expr,
    };
    for f in &subscribe.filters {
        if scope.is_some_and(|s| f.column == s.column) {
            continue; // server injects the real tenant value below
        }
        p = p.and_eq(&f.column, ColumnValue::text(&f.value));
    }

    // Compile the optional safe-SQL-subset expression (ADR-0012) and AND it in.
    // Done BEFORE tenant enforcement so the server-injected tenant clause wraps
    // the client expression — a where_sql can never widen scope past its tenant
    // OR past the rules scope already folded in above.
    if let Some(sql) = &subscribe.where_sql {
        match nostos_domain::parse_predicate_expr(sql) {
            // `Predicate` has no `and(PredicateExpr)` method (only `and_eq`),
            // so fold the parsed expression into the predicate's public `expr`
            // field via `PredicateExpr::and`. Keeps the table binding intact.
            Ok(expr) => {
                p = Predicate {
                    table: p.table,
                    expr: p.expr.and(expr),
                }
            }
            Err(e) => return Err(SubscribeRejection::InvalidWhereSql(e.to_string())),
        }
    }

    // Server-enforced tenant scoping (ADR-0011). Always injected for an
    // authenticated principal when a tenant column is configured — in every
    // rules mode, including `all` (ADR-0031 Global Constraint 11). This stays
    // LAST so it wraps everything above (rules scope + filters + where_sql).
    //
    // ponytail: on a table WITHOUT the tenant column (the deliberately-
    // global shape — e.g. a shared catalog) this predicate references a
    // column the row payloads don't carry, so `PredicateExpr::matches` is
    // false for EVERY event: live changes to such tables never reach
    // tenant-scoped subscribers. The snapshot path handles the shape
    // correctly (`scope_if_column_present` skips the clause); the live path
    // has no column metadata at event-filter time to make the same call.
    // Ceiling: post-seed changes to deliberately-global tables don't stream
    // under tenant deploys — snapshot-first delivery covers seeded catalogs.
    // Upgrade path: tag table schemas (column lists) into the ruleset at
    // compile time so this injection can skip columnless tables exactly
    // like the snapshot path does. The boot-time `audit_tenant_column`
    // guard (nostos-server main) names every table that lands here.
    if let Some(s) = scope {
        p = p.and_eq(s.column, ColumnValue::text(s.value));
    }
    Ok(p)
}

/// Build the server-enforced predicate for a sync stream (P5 design Decision
/// 3). The stream's BOUND template (params already substituted value-level by
/// `predicate_compile::bind_params`) folds in at exactly the `where_sql`
/// seam: rules scope FIRST and fail-closed (a stream on a `NotSynced` table
/// is rejected through the same `RuleDecision` path — design §5), tenant
/// clause LAST so it wraps everything (ADR-0011).
///
/// A param naming the TENANT column narrows here to a fail-closed AND-wrap:
/// `tenant = :rogue AND tenant = <principal>` is the impossible predicate, so
/// an escape attempt yields ZERO rows — never the other tenant's data, and
/// never the principal's rows under a borrowed template (e2e §6 item 3 pins
/// this over real PG). No client filters exist on the stream path: the whole
/// shape is server config.
///
/// # Returns BOTH the session predicate and the snapshot expression
///
/// AUTHORIZATION, not ergonomics. The initial snapshot and live fan-out must
/// enforce the SAME ruleset scope; when they were computed separately the
/// snapshot silently skipped it (audit finding 7). So this returns the pair:
///
/// - `.0` — the full session predicate (rules ∧ bound ∧ tenant) for live
///   fan-out, which filters rows in-process.
/// - `.1` — `rules ∧ bound` for `SnapshotSource::snapshot_stream`, which
///   compiles to SQL. The tenant clause is DELIBERATELY absent: it travels in
///   `snapshot_stream`'s own `tenant` argument, where `scope_if_column_present`
///   can drop it for a deliberately-global table that has no tenant column.
///   Folding tenant into the expr instead would emit `WHERE "org_id" = $1`
///   against a table with no `org_id` — a guaranteed SQL 42703 that the
///   transport swallows into live-fan-out-only (the `products` catalog
///   starvation observed 2026-08-27).
///
/// Returning the pair from ONE function is the point: two call sites deriving
/// the same authorization independently is exactly how finding 7 happened.
pub(super) fn build_stream_predicate(
    table: &str,
    bound: PredicateExpr,
    principal: &Principal,
    tenant_column: Option<&str>,
    ruleset: &ActiveRuleset,
) -> Result<(Predicate, PredicateExpr), SubscribeRejection> {
    let rules_expr = match ruleset.decide(table, principal) {
        RuleDecision::Allow(expr) => expr,
        RuleDecision::DeniedTable => {
            return Err(SubscribeRejection::NotSynced {
                table: table.to_string(),
                mode: ruleset.mode(),
            });
        }
        RuleDecision::DeniedClaim(claim) => {
            return Err(SubscribeRejection::MissingClaim {
                table: table.to_string(),
                claim,
            });
        }
    };
    // `PredicateExpr::and` collapses `Any`, so the zero-config `all` mode
    // (which decides `Allow(Any)`) yields the bare template here rather than
    // `And([Any, template])` — the latter is REFUSED by the SQL compiler and
    // would turn every default deploy's stream snapshot into a swallowed error.
    let snapshot_expr = rules_expr.and(bound);
    let mut p = Predicate {
        table: table.to_string(),
        expr: snapshot_expr.clone(),
    };
    if let Some(s) = principal.tenant_scope(tenant_column) {
        p = p.and_eq(s.column, ColumnValue::text(s.value));
    }
    Ok((p, snapshot_expr))
}

/// Replay-path authorization gate — the op-log twin of the live path's
/// `predicate.matches` filter.
///
/// The live path evaluates EVERY event against the session predicate
/// (`FanOutService::fan_out`), which carries the rules scope and the tenant
/// clause. The replay path reads `nostos_oplog` keyed by tenant ALONE
/// (`OpLogSource::replay_after(tenant, lsn)`) and the socket sink's `admit`
/// gate checks only open/acked/dedup — never a predicate, never a table. So
/// without this function a reconnect delivers every row the TENANT wrote,
/// including tables the ruleset refuses to sync and rows the scope hides:
/// authorization that holds live but not on resume.
///
/// Table check first, then the predicate over the row's own payload. A payload
/// that won't decode fails CLOSED (never over-deliver — same stance as
/// `PredicateExpr::matches` on an unparseable value).
pub(super) fn replay_admits(pred: &nostos_domain::Predicate, ev: &ReplicationEvent) -> bool {
    if ev.op.table() != pred.table {
        return false;
    }
    match &ev.op {
        nostos_domain::RowOp::Insert { payload, .. }
        | nostos_domain::RowOp::Update { payload, .. } => {
            crate::replicator::extract_json_column(payload)
                .is_some_and(|extract| pred.matches(extract))
        }
        // ponytail: a replayed delete carries no old image — `oplog.rs` drops
        // it at read time — so there are no columns to match and the table
        // check is all we have. Ceiling: a client can learn that SOME pk in
        // its own tenant AND its own subscribed table was deleted, even one
        // its row scope would have hidden. Failing closed instead would drop
        // the delete permanently for an offline client, leaving a row that
        // never goes away — the stale-row bug ADR-0014's reconcile boundary
        // exists to prevent, and a worse trade than leaking a pk inside the
        // client's own tenant. Replay-only: live deletes still go through
        // fan-out's predicate. Upgrade path: write the scope columns into
        // `nostos_oplog` at log time so replay can evaluate the predicate
        // exactly like the live path.
        nostos_domain::RowOp::Delete { .. } => true,
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{req, stream_ruleset, toggles_rules};
    use super::*;

    // Task 10: toggles mode, `notes` sync=false → the rules decision is
    // fail-closed, not "everything" — `build_predicate` refuses before any
    // filter/tenant work runs.
    #[test]
    fn subscribe_to_unsynced_table_is_rejected() {
        let ruleset = ActiveRuleset::compile(&toggles_rules("notes", false, None)).unwrap();
        let principal = Principal::new("acct", "tenant-acme");
        let err =
            build_predicate(&req("notes", None, None), &principal, None, &ruleset).unwrap_err();
        assert_eq!(
            err,
            SubscribeRejection::NotSynced {
                table: "notes".into(),
                mode: SyncMode::Toggles,
            }
        );
    }

    // Task 10: rules scope (`status = 'open'`) AND tenant scope (`org_id`) —
    // a row must satisfy both, neither alone is enough.
    #[test]
    fn rules_scope_is_anded_with_tenant_scope() {
        let ruleset =
            ActiveRuleset::compile(&toggles_rules("tasks", true, Some("status = 'open'"))).unwrap();
        let principal = Principal::new("acct", "tenant-acme");
        let predicate = build_predicate(
            &req("tasks", None, None),
            &principal,
            Some("org_id"),
            &ruleset,
        )
        .unwrap();

        let row = |status: &'static str, org: &'static str| {
            move |col: &str| -> Option<ColumnValue> {
                match col {
                    "status" => Some(ColumnValue::text(status)),
                    "org_id" => Some(ColumnValue::text(org)),
                    _ => None,
                }
            }
        };
        assert!(predicate.expr.matches(row("open", "tenant-acme")));
        assert!(
            !predicate.expr.matches(row("closed", "tenant-acme")),
            "rules scope must hold"
        );
        assert!(
            !predicate.expr.matches(row("open", "someone-else")),
            "tenant scope must hold"
        );
    }

    // Task 10 / ADR-0031 Global Constraint 11: `all` mode disables rules but
    // never tenancy — a foreign-tenant row must not match even with no rules
    // scope in play.
    #[test]
    fn all_mode_still_applies_tenant_scope() {
        let ruleset = ActiveRuleset::all_mode();
        let principal = Principal::new("acct", "tenant-acme");
        let predicate = build_predicate(
            &req("tasks", None, None),
            &principal,
            Some("org_id"),
            &ruleset,
        )
        .unwrap();

        let with_org = |org: &'static str| {
            move |col: &str| -> Option<ColumnValue> {
                (col == "org_id").then(|| ColumnValue::text(org))
            }
        };
        assert!(predicate.expr.matches(with_org("tenant-acme")));
        assert!(!predicate.expr.matches(with_org("someone-else")));
    }

    // Task 10: a scope claim the principal doesn't carry is a denial, not a
    // silent widen — `claims.sub` (no explicit claim needed, resolves to the
    // account id) is present, but a table gated on `claims.org_id` with no
    // `org_id` claim on the principal must reject.
    #[test]
    fn missing_claim_rejects_subscribe() {
        let ruleset = ActiveRuleset::compile(&toggles_rules(
            "tasks",
            true,
            Some("org_id = claims.org_id"),
        ))
        .unwrap();
        let principal = Principal::new("acct", "tenant-acme"); // no org_id claim
        let err =
            build_predicate(&req("tasks", None, None), &principal, None, &ruleset).unwrap_err();
        assert_eq!(
            err,
            SubscribeRejection::MissingClaim {
                table: "tasks".into(),
                claim: "org_id".into(),
            }
        );
    }

    // Task 10: client `where_sql` is ANDed onto the rules scope, never
    // substituted for it — `owner_id = claims.sub` (→ "owner1") AND
    // `owner_id = 'someone_else'` is unsatisfiable, so the composed predicate
    // matches nothing, proving where_sql cannot widen past the rules scope.
    #[test]
    fn where_sql_cannot_widen_past_rules() {
        let ruleset =
            ActiveRuleset::compile(&toggles_rules("tasks", true, Some("owner_id = claims.sub")))
                .unwrap();
        let principal = Principal::new("owner1", "tenant-acme");
        let mut subscribe = req("tasks", None, None);
        subscribe.where_sql = Some("owner_id = 'someone_else'".into());
        let predicate = build_predicate(&subscribe, &principal, None, &ruleset).unwrap();

        let with_owner = |owner: &'static str| {
            move |col: &str| -> Option<ColumnValue> {
                (col == "owner_id").then(|| ColumnValue::text(owner))
            }
        };
        assert!(!predicate.expr.matches(with_owner("someone_else")));
        assert!(!predicate.expr.matches(with_owner("owner1")));
    }

    #[test]
    fn stream_predicate_tenant_wrap_is_fail_closed() {
        // build_stream_predicate directly: bound tenant-param + injected
        // tenant clause = unsatisfiable (Decision 3's drop-and-override
        // narrows to AND-wrap here; zero rows, never cross-tenant data).
        let ruleset = stream_ruleset("by_org", "tasks", "org_id = :org");
        let template = nostos_domain::parse_predicate_expr("org_id = :org").unwrap();
        let bound = nostos_domain::predicate_compile::bind_params(
            &template,
            &std::collections::HashMap::from([("org".to_string(), ColumnValue::text("tenant-b"))]),
        )
        .unwrap();
        let principal = Principal::new("acct", "tenant-acme");
        let (p, _snapshot_expr) =
            build_stream_predicate("tasks", bound, &principal, Some("org_id"), &ruleset).unwrap();
        let row = |org: &'static str| {
            move |col: &str| -> Option<ColumnValue> {
                (col == "org_id").then(|| ColumnValue::text(org))
            }
        };
        assert!(!p.matches(row("tenant-b")));
        assert!(!p.matches(row("tenant-acme")));
    }
}
