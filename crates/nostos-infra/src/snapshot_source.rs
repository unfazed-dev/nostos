//! Snapshot-on-subscribe adapter (ADR-0014): read a table's CURRENT rows and
//! hand them to the transport as `Insert` events so a freshly-subscribing
//! client receives pre-existing rows BEFORE live fan-out (PowerSync parity).
//!
//! ## Why this exists
//!
//! Without this, nostos-server is a pure pass-through: a client that connects to
//! an already-populated table receives nothing until the first mutation. The
//! startup snapshot (`replicator::snapshot`) is drained once at boot into the
//! fan-out loop, so it only reaches clients that were connected at startup — a
//! client connecting later misses every pre-existing row. This adapter runs
//! `SELECT * FROM <table>` on subscribe and returns the rows as `Insert`
//! events; the transport delivers them to that one session before live events.
//!
//! ## Trust boundary (security-critical)
//!
//! The table name is CLIENT-CONTROLLED (it comes from the subscribe frame), so
//! it is the one string that reaches SQL. The same three-defense discipline
//! `write_back.rs` uses applies, reduced for a read-only path:
//!
//! 1. **Identifier regex FIRST.** The table is validated against
//!    `^[a-z_][a-z0-9_]*$` before any SQL is built. A name that isn't a bare
//!    lowercase identifier is rejected — it can never reach the query builder.
//! 2. **Quoted identifier.** The validated name is emitted wrapped in `"…"`
//!    (with embedded `"` doubled), so even a hypothetical regex bypass would be
//!    contained by the quote.
//! 3. **No value interpolation.** Snapshot reads values; it never binds
//!    client-supplied values. There is no `$1…$n` surface here, so the
//!    value-injection defense from the write path is moot. The SELECT is a
//!    bare `SELECT "<cols>"::text, … FROM "<table>"` with zero parameters.
//!
//! Column NAMES are NOT client-controlled (they come from the prepared
//! statement's metadata via `pg_attribute`, not the subscribe frame), but they
//! are quoted anyway — defense in depth, same as `replicator::snapshot`.
//!
//! ## Payload fidelity (ADR-0019)
//!
//! Each row's payload is built with the SAME `append_typed_value` OID-keyed
//! mapping the streaming path (`pg::tuple_to_json_payload`) and the startup
//! snapshot (`snapshot::build_json_payload`) use, so a snapshot row is
//! byte-for-byte indistinguishable from a streamed insert. The column OIDs come
//! from the prepared statement's metadata; each cell is fetched as Postgres
//! TEXT (via `::text` cast) — the exact wire form `append_typed_value` expects.
//!
//! `unsafe` is forbidden crate-wide. This module performs no I/O of its own
//! beyond a single read-only `SELECT` per subscribe.

#![cfg(feature = "pg")]

use std::sync::Arc;
use std::sync::OnceLock;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::Mutex;

use nostos_application::ports::{SnapshotError, SnapshotSource};
use nostos_domain::{
    ColumnValue, Lsn, PredicateExpr, PredicateFilter, ReplicationEvent, RowOp, TenantScope,
};

/// Reuse the streaming path's OID-keyed JSON mapping (ADR-0019) so snapshot
/// rows render byte-identically to streamed rows. Both helpers are `pub(crate)`
/// in the replicator module — reusing them (rather than re-implementing) is
/// what guarantees a snapshot row and a streamed insert of the same row
/// produce the same payload bytes.
use crate::replicator::pg::json_escape_into;
use crate::replicator::typed::append_typed_value;

/// v1 convention: the primary-key column is fixed to `id`. Mirrors
/// `PgWriteBack`'s `PK_COLUMN`. ponytail: discover the pk from `pg_constraint`
/// when a design partner needs composite / renamed primary keys.
const PK_COLUMN: &str = "id";

/// A `SnapshotSource` that reads current rows from the source Postgres via a
/// pool-of-one `tokio_postgres::Client` (mirrors `PgWriteBack`'s lazy-connect
/// discipline). Constructed by the composition root under
/// `NOSTOS_REPLICATOR=pg`; injected into the transport via `.with_snapshotter`.
///
/// ponytail: per-subscribe `SELECT *` cost + whole-table buffered in memory.
/// Ceiling: one PG round-trip + one full table scan per subscribe, which is
/// fine for the OSS self-host / single-tenant dev target (small tables, low
/// connect rate). Upgrade path: an in-memory materialized view maintained by
/// the live fan-out events (each upsert/delete updates the view; a subscribe
/// reads the view with no PG round-trip). Build the view when a real load
/// number says the per-subscribe SELECT is the bottleneck — not before.
pub struct PgSnapshotter {
    pg_url: String,
    /// Pool-of-one. `Mutex` (not `OnceCell`) so a dead connection can be
    /// replaced transparently on the next subscribe — same discipline as
    /// `PgWriteBack`.
    client: Arc<Mutex<Option<tokio_postgres::Client>>>,
}

impl PgSnapshotter {
    /// Construct with a libpq-style URL. Does NOT connect — the first
    /// subscribe opens the connection lazily (and reopens it transparently if
    /// it ever dies).
    #[must_use]
    pub fn new(pg_url: &str) -> Self {
        Self {
            pg_url: pg_url.to_string(),
            client: Arc::new(Mutex::new(None)),
        }
    }

    /// Obtain a connected client, opening the connection lazily if none is
    /// cached (identical to `PgWriteBack::client`).
    async fn client(&self) -> Result<tokio_postgres::Client, SnapshotError> {
        let mut guard = self.client.lock().await;
        if let Some(c) = guard.take() {
            return Ok(c);
        }
        crate::pg_connect::pg_connect_bounded(&self.pg_url)
            .await
            .map_err(SnapshotError::Backend)
    }

    /// Return a client to the pool after a successful read.
    async fn return_client(&self, client: tokio_postgres::Client) {
        let mut guard = self.client.lock().await;
        *guard = Some(client);
    }

    /// Drop the client slot after an error that may have killed the
    /// connection. The next subscribe reopens.
    async fn drop_client(&self) {
        let mut guard = self.client.lock().await;
        *guard = None;
    }

    /// Prepare `SELECT * FROM <table>` and read the column names + type OIDs
    /// from the statement's metadata (zero rows needed — a prepared statement
    /// carries its column descriptors without executing). The OIDs drive the
    /// OID-keyed JSON mapping so the snapshot payload is byte-identical to
    /// the streaming path's (ADR-0019). Shared by `snapshot` and
    /// `snapshot_stream`.
    async fn prepare_columns(
        &self,
        client: &tokio_postgres::Client,
        quoted_table: &str,
    ) -> Result<Vec<(String, i32)>, SnapshotError> {
        let prep = match client
            .prepare(&format!("SELECT * FROM {quoted_table}"))
            .await
        {
            Ok(s) => s,
            Err(e) => {
                self.drop_client().await;
                return Err(SnapshotError::Backend(format!("prepare: {e}")));
            }
        };
        // (name, type OID). OIDs are u32 on the wire; append_typed_value takes
        // i32 (Postgres OIDs are always < 2^31). A convert failure yields -1,
        // which append_typed_value treats as "unrecognized" → string
        // passthrough — never a panic, never a dropped row.
        Ok(prep
            .columns()
            .iter()
            .map(|c| {
                let oid = i32::try_from(c.type_().oid()).unwrap_or(-1);
                (c.name().to_string(), oid)
            })
            .collect())
    }
}

#[async_trait]
impl SnapshotSource for PgSnapshotter {
    async fn snapshot(
        &self,
        table: &str,
        base_lsn: Lsn,
        tenant: Option<TenantScope<'_>>,
    ) -> Result<Vec<ReplicationEvent>, SnapshotError> {
        // 1. TRUST BOUNDARY: validate the table identifier BEFORE any SQL is
        //    built. The name is client-controlled (subscribe frame); the regex
        //    rejects anything that isn't a bare lowercase identifier, so it can
        //    never break out of the identifier-quote the builder adds.
        if let Err(bad) = validate_ident(table) {
            return Err(SnapshotError::InvalidTable(bad));
        }
        let quoted_table = quote_ident(table);

        let client = self.client().await?;

        // 2. Column names + type OIDs from a prepared statement's metadata
        //    (zero rows needed). The OIDs drive the OID-keyed JSON mapping so
        //    the snapshot payload is byte-identical to the streaming path's
        //    (ADR-0019).
        let cols = self.prepare_columns(&client, &quoted_table).await?;

        // 3. Build the text-cast SELECT. Every column is cast to `::text` so
        //    each cell comes back as the Postgres TEXT wire form — the exact
        //    shape `append_typed_value` consumes (same as COPY's text output,
        //    which is what `replicator::snapshot` parses). Column names came
        //    from the catalog (trusted), but quote them anyway — defense in
        //    depth, same as the startup snapshot's `quote_ident`.
        let select_list = select_list_text(&cols);
        // Tenant scoping (the trait's enforced contract): when the
        // connection's principal carries a TenantScope, restrict the
        // snapshot to that tenant — but ONLY when the table actually HAS
        // the tenant column (`scope_if_column_present` below): a tenant-
        // column deploy with a deliberately global table (no tenant
        // column — e.g. a shared catalog beside tenant-scoped tables, the
        // shape nostos_rules.toml hand-mode documents) must not get the
        // clause. `WHERE "<col>"::text = $1` against a columnless table
        // is a guaranteed SQL 42703, which silently downgraded every
        // subscribe of that table to live-fan-out-only (observed
        // 2026-08-27: the atlet `products` catalog starved the shop).
        // Skipping leaks nothing: a table without the column has no
        // tenant data to scope, and table-level access stays the rules
        // engine's decision. The column name is operator config
        // (trusted) and is identifier-quoted; the value is BOUND as $1,
        // never interpolated — mirrors the read path's server-injected
        // tenant clause (ADR-0011). None = anonymous / single-tenant
        // deployment, where an unfiltered snapshot is legitimate.
        // Bind target declared at function scope so the parameter
        // reference outlives the query call (the match arms' copies of the
        // Copy scope would not).
        let tenant = scope_if_column_present(&cols, tenant);
        let tenant_value: Option<&str> = tenant.map(|s| s.value);
        let sql = match tenant {
            // `::text` on the COLUMN side: the bound value is always a
            // string (JWT claim), while tenant columns may be uuid/int —
            // `uuid = text` has no operator and would error. Casting
            // the column compares canonical text forms, uniform across
            // types (an index on the column is bypassed — same trade the
            // live path's literal-coercion clause avoids, accepted here
            // for bind-safety; snapshots are per-subscribe, not hot).
            Some(scope) => format!(
                "SELECT {select_list} FROM {quoted_table} WHERE {}::text = $1",
                quote_ident(scope.column)
            ),
            None => format!("SELECT {select_list} FROM {quoted_table}"),
        };
        let mut params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = Vec::new();
        if let Some(v) = &tenant_value {
            params.push(v);
        }
        let rows = match client.query(&sql, &params).await {
            Ok(r) => r,
            Err(e) => {
                self.drop_client().await;
                return Err(SnapshotError::Backend(format!(
                    "query: {}",
                    display_chain(&e)
                )));
            }
        };

        // 4. One Insert event per row, stamped above `base_lsn` (see
        //    `rows_to_events` for the gate-safety argument).
        let events = rows_to_events(table, &cols, &rows, base_lsn);

        self.return_client(client).await;
        Ok(events)
    }

    /// P5 sync streams (design §3): a TARGETED per-stream snapshot. The
    /// stream's bound predicate compiles to `WHERE <shape>` with every VALUE
    /// as a positional `$n` bind (Decision 2's fourth defense — no client
    /// byte and no template literal is ever interpolated), and the tenant
    /// clause appends from the principal exactly as in `snapshot` — closing,
    /// for stream snapshots, the old unfiltered-snapshot ponytail
    /// (`ports.rs:633-638`) at this seam too.
    async fn snapshot_stream(
        &self,
        table: &str,
        predicate: &PredicateExpr,
        base_lsn: Lsn,
        tenant: Option<TenantScope<'_>>,
    ) -> Result<Vec<ReplicationEvent>, SnapshotError> {
        // 1. Same trust boundary as `snapshot`: the table identifier first.
        if let Err(bad) = validate_ident(table) {
            return Err(SnapshotError::InvalidTable(bad));
        }
        let quoted_table = quote_ident(table);

        // 2. Compile the predicate to WHERE text + binds BEFORE touching the
        //    connection — pure and fail-fast (an unbound `Param` leaf is an
        //    error here, never a widened snapshot).
        let (mut where_sql, mut binds) = compile_stream_where(predicate)?;

        let client = self.client().await?;
        let cols = self.prepare_columns(&client, &quoted_table).await?;
        let select_list = select_list_text(&cols);

        // 3. Tenant clause LAST, bound, with the same `::text` column cast as
        //    `snapshot` (JWT claims are strings; tenant columns may be
        //    uuid/int). `AND`ed outside the template's own parentheses.
        //    Same column-presence gate as `snapshot`: a deliberately global
        //    table (no tenant column) must not get the clause — it is a
        //    guaranteed SQL 42703 otherwise (see scope_if_column_present).
        let tenant = scope_if_column_present(&cols, tenant);
        let tenant_value: Option<String> = tenant.map(|s| s.value.to_string());
        if let (Some(scope), Some(value)) = (tenant, tenant_value.as_ref()) {
            binds.push(ColumnValue::Text(value.clone()));
            where_sql = format!(
                "({where_sql}) AND {}::text = ${}",
                quote_ident(scope.column),
                binds.len()
            );
        }
        let sql = format!("SELECT {select_list} FROM {quoted_table} WHERE {where_sql}");
        let params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
            binds.iter().map(column_value_as_sql).collect();
        let rows = match client.query(&sql, &params).await {
            Ok(r) => r,
            Err(e) => {
                self.drop_client().await;
                return Err(SnapshotError::Backend(format!(
                    "query: {}",
                    display_chain(&e)
                )));
            }
        };

        let events = rows_to_events(table, &cols, &rows, base_lsn);
        self.return_client(client).await;
        Ok(events)
    }
}

/// The `SELECT` list with every column `::text`-cast (see `snapshot` step 3).
fn select_list_text(cols: &[(String, i32)]) -> String {
    cols.iter()
        .map(|(n, _)| format!("{}::text", quote_ident(n)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The tenant scope to ACTUALLY apply: `Some` only when the table's own
/// column list contains the tenant column. A tenant-column deploy with a
/// deliberately global table (no tenant column — a shared catalog beside
/// tenant-scoped tables, the shape `nostos_rules.toml` hand-mode documents)
/// must not get the clause: `WHERE "<col>"::text = $1` against a columnless
/// table is a guaranteed SQL 42703, which the transport logs as a swallowed
/// `snapshot backend: query: db error` and downgrades to live-fan-out-only
/// (observed 2026-08-27: the atlet `products` catalog starved the shop, and
/// with a default-`org_id` column EVERY table 42703'd — no snapshots, no
/// live matches, no doorbells). Skipping leaks nothing: a table without the
/// column has no tenant data to scope, and table-level access remains the
/// rules engine's decision. `None` passes through unchanged.
fn scope_if_column_present<'a>(
    cols: &[(String, i32)],
    tenant: Option<TenantScope<'a>>,
) -> Option<TenantScope<'a>> {
    tenant.filter(|s| cols.iter().any(|(n, _)| n == s.column))
}

/// Render an error WITH its full source chain. `tokio_postgres::Error`'s
/// Display is just `db error` — the SQLSTATE, the driver message, and the
/// server's own text (the part that names the missing column) all live in
/// `source()` levels, which `format!("{e}")` silently drops. The 2026-08-27
/// products starvation cost an hour precisely because the transport logged
/// `snapshot backend: query: db error` and the 42703 detail never reached
/// the operator. Every snapshot query error renders through this instead.
fn display_chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut out = e.to_string();
    let mut cur = e.source();
    while let Some(src) = cur {
        // Skip levels that add nothing (some wrappers re-Display their
        // source verbatim) so the chain reads as new information only.
        let text = src.to_string();
        if !text.is_empty() && !out.contains(&text) {
            out.push_str(": ");
            out.push_str(&text);
        }
        cur = src.source();
    }
    out
}

/// Build one Insert event per row. Each gets a UNIQUE LSN strictly above
/// `base_lsn` so the per-session sink's LSN gate (`TokioEventSink::deliver`
/// drops `lsn <= acked && acked != 0`, and the dedup ring drops exact-LSN
/// duplicates) does NOT swallow snapshot rows. For a fresh client base_lsn =
/// 0, so events are stamped 1, 2, 3, … — far below any real WAL LSN, so live
/// fan-out events (which carry real Postgres WAL positions, typically in the
/// millions+) always sort above the synthetic range and are never mis-dropped.
///
/// ponytail: LSNs share the single u64 space with real WAL LSNs. For a
/// RESUMING client (base_lsn = a real WAL position N), synthetic LSNs
/// N+1..N+M could in principle collide with a real WAL event in that narrow
/// window, dropping one live event. This is not attacker-reachable and the
/// primary target case (fresh subscribe, base_lsn = 0) is unaffected.
/// Upgrade path: a per-session synthetic LSN band (e.g. reserve a high bit),
/// or skip-snapshot-on-resume (a resuming client already has the table state
/// from its prior session). Revisit when a resume-mode design partner
/// reports it.
fn rows_to_events(
    table: &str,
    cols: &[(String, i32)],
    rows: &[tokio_postgres::Row],
    base_lsn: Lsn,
) -> Vec<ReplicationEvent> {
    // PK column index (v1: the column named "id"). ponytail: read from
    // pg_constraint for composite/renamed PKs (mirrors PgWriteBack).
    let pk_index = cols.iter().position(|(n, _)| n == PK_COLUMN);
    let mut events = Vec::with_capacity(rows.len());
    for (i, row) in rows.iter().enumerate() {
        let payload = build_payload(cols, row);
        // PK: the "id" column's text value if present, else the row index
        // (a stable fallback so the event is still well-formed; the client
        // upserts by pk and a missing-id table is out of v1 scope).
        let pk = pk_index
            .and_then(|idx| row.try_get::<usize, Option<String>>(idx).ok().flatten())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| i.to_string());
        let lsn = Lsn::new(base_lsn.raw().saturating_add(1).saturating_add(i as u64));
        events.push(ReplicationEvent::new(
            lsn,
            RowOp::Insert {
                table: table.to_string(),
                pk,
                payload: Bytes::from(payload),
            },
        ));
    }
    events
}

/// Compile a fully-bound stream predicate into SQL `WHERE` text + positional
/// binds (P5 sync streams, design Decision 2/§3).
///
/// - Every VALUE — template literal or `:param`-bound — becomes a `$n` bind;
///   nothing is ever interpolated into the SQL text.
/// - The SHAPE (column names, operators, boolean structure) comes from the
///   operator-authored template (trusted config); column identifiers are
///   STILL validated against the strict identifier regex and quoted —
///   belt-and-braces, same discipline as the table name.
/// - Typed rendering: Number/Float/Bool bind natively and compare against
///   the raw column. A `Text` value renders `"col"::text <op> $n` — the same
///   column-side cast the tenant clause uses, because string params (e.g.
///   JWT-derived claims) must compare cleanly against uuid/int columns. The
///   index-bypass trade is accepted exactly as documented on `snapshot`'s
///   tenant clause: stream snapshots are per-subscribe, not hot.
/// - A surviving `Param` or `Any` leaf, or a bare `Any` root, is a loud
///   error — the transport's bind step must have replaced every placeholder
///   before this point.
fn compile_stream_where(expr: &PredicateExpr) -> Result<(String, Vec<ColumnValue>), SnapshotError> {
    let mut binds = Vec::new();
    let sql = compile_expr(expr, &mut binds)?;
    Ok((sql, binds))
}

fn bad_template(reason: &str) -> SnapshotError {
    SnapshotError::Backend(format!("stream predicate: {reason}"))
}

fn compile_expr(
    expr: &PredicateExpr,
    binds: &mut Vec<ColumnValue>,
) -> Result<String, SnapshotError> {
    match expr {
        PredicateExpr::Eq(f) => compile_leaf(f, "=", binds),
        PredicateExpr::Ne(f) => compile_leaf(f, "!=", binds),
        PredicateExpr::Lt(f) => compile_leaf(f, "<", binds),
        PredicateExpr::Gt(f) => compile_leaf(f, ">", binds),
        PredicateExpr::Le(f) => compile_leaf(f, "<=", binds),
        PredicateExpr::Ge(f) => compile_leaf(f, ">=", binds),
        PredicateExpr::And(parts) | PredicateExpr::Or(parts) => {
            if parts.is_empty() {
                return Err(bad_template("empty boolean group"));
            }
            let joiner = if matches!(expr, PredicateExpr::And(_)) {
                " AND "
            } else {
                " OR "
            };
            let mut sqls = Vec::with_capacity(parts.len());
            for p in parts {
                sqls.push(format!("({})", compile_expr(p, binds)?));
            }
            Ok(sqls.join(joiner))
        }
        PredicateExpr::Not(inner) => Ok(format!("NOT ({})", compile_expr(inner, binds)?)),
        // The grammar's parser never yields `Any` (every leaf is a
        // comparison); if one arrives here the caller built the tree by
        // hand — refuse rather than snapshot the whole table.
        PredicateExpr::Any => Err(bad_template(
            "match-all is not a valid stream template; a stream must narrow",
        )),
    }
}

fn compile_leaf(
    f: &PredicateFilter,
    op: &str,
    binds: &mut Vec<ColumnValue>,
) -> Result<String, SnapshotError> {
    if let Err(bad) = validate_ident(&f.column) {
        return Err(bad_template(&format!(
            "column identifier {bad:?} fails the strict regex"
        )));
    }
    let quoted_col = quote_ident(&f.column);
    match &f.value {
        ColumnValue::Param(name) => Err(bad_template(&format!(
            "unbound placeholder :{name} survived to snapshot time (bind_params must run first)"
        ))),
        ColumnValue::Any => Err(bad_template("wildcard `Any` is not a stream value")),
        // String values compare against the column's canonical TEXT form so
        // string params work against uuid/int columns (see fn docs).
        value @ ColumnValue::Text(_) => {
            binds.push(value.clone());
            Ok(format!("{quoted_col}::text {op} ${}", binds.len()))
        }
        value => {
            binds.push(value.clone());
            Ok(format!("{quoted_col} {op} ${}", binds.len()))
        }
    }
}

/// Borrow a [`ColumnValue`] as a `tokio_postgres` bind parameter. Number →
/// i64, Float → f64, Bool → bool, Text → &str — each already implements
/// `ToSql`. `Param`/`Any` never reach here (compile_leaf rejects them).
fn column_value_as_sql(v: &ColumnValue) -> &(dyn tokio_postgres::types::ToSql + Sync) {
    match v {
        ColumnValue::Number(n) => n,
        ColumnValue::Float(f) => f,
        ColumnValue::Bool(b) => b,
        ColumnValue::Text(s) => s,
        ColumnValue::Param(_) | ColumnValue::Any => {
            unreachable!("compile_leaf rejects Param/Any before binds are collected")
        }
    }
}

/// Build the JSON payload for one row, byte-identical to the streaming path's
/// `tuple_to_json_payload` / the startup snapshot's `build_json_payload`: every
/// column goes through the SAME `append_typed_value`, keyed by its type OID,
/// and every column name goes through the SAME `json_escape_into`. A SQL NULL
/// cell (`Option<String>::None`) renders as JSON `null` — not a fabricated
/// `""`/`false`/`0` — exactly like the streaming path.
fn build_payload(cols: &[(String, i32)], row: &tokio_postgres::Row) -> Vec<u8> {
    let mut out = String::with_capacity(128);
    out.push('{');
    for (i, (name, oid)) in cols.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('"');
        json_escape_into(&mut out, name);
        out.push_str("\":");
        // ::text cast in the SELECT makes every column a TEXT value; NULL stays
        // NULL (Option::None). try_get never panics on a type mismatch — a
        // failure here would be a Postgres protocol bug, so flatten to None
        // (renders as JSON null) rather than crashing the subscribe path.
        let cell = row.try_get::<usize, Option<String>>(i).ok().flatten();
        append_typed_value(&mut out, *oid, cell.as_deref());
    }
    out.push('}');
    out.into_bytes()
}

// ---------------------------------------------------------------------------
// Identifier defense — identical to `write_back.rs::pg`'s helpers. Duplicated
// rather than shared because the helpers are private to write_back's `pg`
// submodule and the snapshot path is a distinct port (snapshot_source.rs).
// Keeping a second copy is deliberate: the two ports must not gain a hidden
// coupling through a shared private helper module, and the copies are tiny
// (one regex + a one-line quote). If a THIRD caller appears, lift these into a
// shared `crate::ident` module at that point.
// ---------------------------------------------------------------------------

/// The strict identifier regex: a bare lowercase SQL identifier
/// (`^[a-z_][a-z0-9_]*$`). Applied to the table name before any SQL is built.
/// This is the SAME pattern `PgWriteBack` uses — the structural injection
/// defense for a client-controlled identifier.
fn ident_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"^[a-z_][a-z0-9_]*$")
            .expect("identifier regex is a valid static pattern")
    })
}

/// Validate one identifier against the strict regex. Returns `Ok(())` if it
/// matches, or `Err` with the offending identifier so the caller can wrap it
/// in a [`SnapshotError::InvalidTable`].
fn validate_ident(name: &str) -> Result<(), String> {
    if ident_regex().is_match(name) {
        Ok(())
    } else {
        Err(name.to_string())
    }
}

/// Wrap a validated identifier in Postgres double-quotes (the identifier
/// quote). The caller MUST have run [`validate_ident`] first — this function
/// does not re-check, it only quotes (belt-and-braces on top of the regex).
fn quote_ident(name: &str) -> String {
    // Postgres escapes a literal `"` inside an identifier by doubling it. The
    // regex already guarantees no `"` can be present, but doubling is correct
    // regardless and costs nothing.
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The result of a boot-time [`audit_tenant_column`] pass: how each requested
/// table relates to the configured tenant column.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TenantColumnAudit {
    /// Tables that EXIST but have NO column named like the tenant column —
    /// the "deliberately global table" shape (a shared catalog beside
    /// tenant-scoped tables). Snapshots skip the tenant clause for these
    /// (see [`scope_if_column_present`]); the LIVE path's predicate still
    /// references the column and matches nothing under tenant deploys —
    /// an operator must KNOW a table lands here.
    pub columnless: Vec<String>,
    /// Tables that do not exist AT ALL in the `public` schema. These fail
    /// loudly on first snapshot anyway; listed so the operator sees the
    /// ruleset naming tables the database never had (usually a typo).
    pub missing: Vec<String>,
}

/// Boot-time operator guard for tenant-column deploys: classify the given
/// tables by whether they actually carry the configured tenant column.
/// Motivated by the 2026-08-27 incident — a deploy with
/// `NOSTOS_TENANT_COLUMN` set (especially via the clap default `org_id`)
/// against tables without that column produced only swallowed snapshot
/// errors and starving shops; one boot log line naming the table + column
/// turns that incident into a one-read diagnosis.
///
/// Reads `information_schema` only (no table locks, no per-table queries —
/// two catalog scans regardless of table count). Failures are the CALLER's
/// to tolerate: a guard must not be able to block boot, so callers log the
/// error and continue.
///
/// # Errors
/// `tokio_postgres` connection/query errors verbatim.
pub async fn audit_tenant_column(
    pg_url: &str,
    column: &str,
    tables: &[String],
) -> Result<TenantColumnAudit, tokio_postgres::Error> {
    let (client, conn) = tokio_postgres::connect(pg_url, tokio_postgres::NoTls).await?;
    let conn_task = tokio::spawn(async move {
        let _ = conn.await;
    });

    let existing: Vec<String> = client
        .query(
            "SELECT table_name FROM information_schema.tables \
             WHERE table_schema = 'public' AND table_type = 'BASE TABLE'",
            &[],
        )
        .await?
        .iter()
        .map(|row| row.get::<_, String>(0))
        .collect();
    let with_column: Vec<String> = client
        .query(
            "SELECT table_name FROM information_schema.columns \
             WHERE table_schema = 'public' AND column_name = $1",
            &[&column],
        )
        .await?
        .iter()
        .map(|row| row.get::<_, String>(0))
        .collect();
    conn_task.abort();

    let mut audit = TenantColumnAudit::default();
    for table in tables {
        if !existing.contains(table) {
            audit.missing.push(table.clone());
        } else if !with_column.contains(table) {
            audit.columnless.push(table.clone());
        }
    }
    Ok(audit)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope_for(column: &'static str) -> TenantScope<'static> {
        TenantScope::new(column, "t1")
    }

    #[test]
    fn tenant_scope_applies_when_column_present() {
        let cols = vec![("id".to_string(), 25), ("user_id".to_string(), 25)];
        let gated = scope_if_column_present(&cols, Some(scope_for("user_id")));
        assert!(gated.is_some());
        assert_eq!(gated.map(|s| s.column), Some("user_id"));
    }

    #[test]
    fn tenant_scope_skips_deliberately_global_table() {
        // The products-catalog shape: table has NO tenant column — the
        // clause must be dropped, not 42703 the whole snapshot.
        let cols = vec![("id".to_string(), 25), ("name".to_string(), 25)];
        assert!(scope_if_column_present(&cols, Some(scope_for("user_id"))).is_none());
    }

    #[test]
    fn tenant_scope_none_passthrough() {
        assert!(scope_if_column_present(&[("user_id".to_string(), 25)], None).is_none());
    }

    /// The incident shape: a top-level error whose Display is generic
    /// ("db error") with the actionable text one source level down.
    #[test]
    fn display_chain_reaches_every_source_level() {
        use std::error::Error;
        use std::fmt;

        #[derive(Debug)]
        struct Leaf(&'static str);
        impl fmt::Display for Leaf {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }
        impl Error for Leaf {}

        #[derive(Debug)]
        struct Middle {
            top: &'static str,
            src: Leaf,
        }
        impl fmt::Display for Middle {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.top)
            }
        }
        impl Error for Middle {
            fn source(&self) -> Option<&(dyn Error + 'static)> {
                Some(&self.src)
            }
        }

        let e = Middle {
            top: "db error",
            src: Leaf("ERROR: column \"user_id\" does not exist"),
        };
        assert_eq!(
            display_chain(&e),
            "db error: ERROR: column \"user_id\" does not exist"
        );
    }

    #[test]
    fn display_chain_skips_duplicate_levels() {
        use std::error::Error;
        use std::fmt;

        #[derive(Debug)]
        struct Leaf(&'static str);
        impl fmt::Display for Leaf {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }
        impl Error for Leaf {}

        #[derive(Debug)]
        struct Wrapper;
        impl fmt::Display for Wrapper {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "same text")
            }
        }
        impl Error for Wrapper {
            fn source(&self) -> Option<&(dyn Error + 'static)> {
                Some(&Leaf("same text"))
            }
        }

        // A wrapper that re-Displays its source verbatim must not render
        // the duplicate level twice.
        assert_eq!(display_chain(&Wrapper), "same text");
    }

    #[test]
    fn validate_ident_accepts_bare_lowercase() {
        assert!(validate_ident("tasks").is_ok());
        assert!(validate_ident("org_id").is_ok());
        assert!(validate_ident("_private").is_ok());
        assert!(validate_ident("col2").is_ok());
    }

    #[test]
    fn validate_ident_rejects_injection_attempts() {
        // The security-critical cases: anything that isn't a bare lowercase
        // identifier must be rejected before it can reach quote_ident().
        assert!(validate_ident("a; DROP TABLE x").is_err());
        assert!(validate_ident("col\"--").is_err());
        assert!(validate_ident("Title").is_err()); // uppercase
        assert!(validate_ident("col name").is_err()); // space
        assert!(validate_ident("schema.table").is_err()); // dot
        assert!(validate_ident("1col").is_err()); // leading digit
        assert!(validate_ident("").is_err()); // empty
        assert!(validate_ident("café").is_err()); // non-ascii
    }

    #[test]
    fn quote_ident_doubles_embedded_quotes() {
        assert_eq!(quote_ident("tasks"), "\"tasks\"");
        // A hypothetical escaped quote — doubled, not backslashed. The regex
        // guarantees none reaches here in production; correct regardless.
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
    }

    #[test]
    fn lsn_assignment_is_strictly_above_base_and_unique() {
        // Mirror the snapshot() LSN math to assert the gate-safety invariant
        // without a database: every event LSN must be > base and all unique.
        let base = Lsn::new(100);
        let row_count = 5_usize;
        let mut lsns: Vec<u64> = (0..row_count)
            .map(|i| base.raw().saturating_add(1).saturating_add(i as u64))
            .collect();
        assert!(lsns.iter().all(|&l| l > base.raw()));
        lsns.sort_unstable();
        lsns.dedup();
        assert_eq!(lsns.len(), row_count, "snapshot LSNs must be unique");
    }

    #[test]
    fn fresh_subscribe_base_zero_starts_at_one() {
        // The primary target case: fresh client, base_lsn = 0. First event
        // must be 1 (strictly > 0 AND non-zero so the sink's `acked != 0`
        // guard doesn't even apply, but >0 is required to stay clear of the
        // dedup ring's 0 sentinel behavior).
        let base = Lsn::new(0);
        let first = base.raw().saturating_add(1).saturating_add(0);
        assert_eq!(first, 1);
    }

    // ---- P5: compile_stream_where (pure; the §6 unit coverage for §3) ----

    fn bound(template: &str, params: &[(&str, ColumnValue)]) -> PredicateExpr {
        let expr = nostos_domain::parse_predicate_expr(template).unwrap();
        let map: std::collections::HashMap<String, ColumnValue> = params
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect();
        nostos_domain::predicate_compile::bind_params(&expr, &map).unwrap()
    }

    #[test]
    fn stream_where_binds_every_value_positionally() {
        let expr = bound(
            "owner_id = :owner AND priority >= :min",
            &[
                ("owner", ColumnValue::text("u1")),
                ("min", ColumnValue::number(3)),
            ],
        );
        let (sql, binds) = compile_stream_where(&expr).unwrap();
        assert_eq!(sql, "(\"owner_id\"::text = $1) AND (\"priority\" >= $2)");
        assert_eq!(binds, vec![ColumnValue::text("u1"), ColumnValue::number(3)]);
    }

    #[test]
    fn stream_where_all_six_operators() {
        for (tpl, op) in [
            ("a = :p", "="),
            ("a != :p", "!="),
            ("a < :p", "<"),
            ("a > :p", ">"),
            ("a <= :p", "<="),
            ("a >= :p", ">="),
        ] {
            let expr = bound(tpl, &[("p", ColumnValue::number(1))]);
            let (sql, binds) = compile_stream_where(&expr).unwrap();
            assert_eq!(sql, format!("\"a\" {op} $1"), "template {tpl}");
            assert_eq!(binds, vec![ColumnValue::number(1)]);
        }
    }

    #[test]
    fn stream_where_parenthesizes_or_and_not() {
        let expr = bound(
            "NOT (a = :x OR b = :y)",
            &[
                ("x", ColumnValue::number(1)),
                ("y", ColumnValue::boolean(true)),
            ],
        );
        let (sql, _) = compile_stream_where(&expr).unwrap();
        assert_eq!(sql, "NOT ((\"a\" = $1) OR (\"b\" = $2))");
    }

    #[test]
    fn stream_where_text_values_get_column_text_cast() {
        // String params must compare cleanly against uuid/int columns — the
        // same column-side ::text cast the tenant clause uses.
        let expr = bound("owner_id = :owner", &[("owner", ColumnValue::text("u1"))]);
        let (sql, _) = compile_stream_where(&expr).unwrap();
        assert_eq!(sql, "\"owner_id\"::text = $1");
        // Numbers/bools/floats bind natively, no cast (numeric ordering is
        // exact — a lexical ::text compare would invert 9 vs 10).
        let expr = bound("priority >= :min", &[("min", ColumnValue::number(3))]);
        let (sql, _) = compile_stream_where(&expr).unwrap();
        assert_eq!(sql, "\"priority\" >= $1");
    }

    #[test]
    fn stream_where_rejects_unbound_placeholder_leaf() {
        // A hand-built tree (never bound) must error loudly, never widen.
        let expr = PredicateExpr::eq("owner", ColumnValue::param("owner"));
        let err = compile_stream_where(&expr).expect_err("unbound Param must error");
        assert!(err.to_string().contains(":owner"), "got: {err}");
    }

    #[test]
    fn stream_where_rejects_any_root_and_wildcard_leaf() {
        let err = compile_stream_where(&PredicateExpr::Any).expect_err("Any root must error");
        assert!(err.to_string().contains("match-all"), "got: {err}");
        let expr = PredicateExpr::eq("owner", ColumnValue::Any);
        assert!(compile_stream_where(&expr).is_err());
    }

    #[test]
    fn stream_where_rejects_bad_column_identifier() {
        // A hand-built tree with a non-regex column never reaches SQL.
        let expr = PredicateExpr::eq("owner\"; DROP TABLE tasks;--", ColumnValue::number(1));
        assert!(compile_stream_where(&expr).is_err());
    }

    #[test]
    fn stream_where_binds_literals_too() {
        // Template literals (operator-authored) are bound, not interpolated —
        // one uniform defense, no literal-to-SQL rendering at all.
        let expr = nostos_domain::parse_predicate_expr("status = 'open' AND pri > 2").unwrap();
        let (sql, binds) = compile_stream_where(&expr).unwrap();
        assert_eq!(sql, "(\"status\"::text = $1) AND (\"pri\" > $2)");
        assert_eq!(
            binds,
            vec![ColumnValue::text("open"), ColumnValue::number(2)]
        );
    }
}
