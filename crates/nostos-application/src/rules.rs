//! Compiles a validated `nostos_rules.toml` (`nostos_domain::SyncRules`) into a
//! per-session decision engine (ADR-0031).
//!
//! [`SyncRules`] is data: the on-disk shape plus structural/scope validation
//! and a checksum. [`ActiveRuleset`] is the compiled form the transport
//! actually consults on every subscribe — parsing happens once, at load, not
//! on the hot path. It selects the section matching the ruleset's
//! [`SyncMode`] (`toggles` vs `hand`) once, at compile time, so the inactive
//! section is dropped and never consulted again.
//!
//! Fail-closed (ADR-0031, Global Constraint 10): every ambiguity — an
//! unlisted table, a toggled-off table, a scope referencing a claim the
//! connecting principal doesn't carry — resolves to a deny variant, never to
//! [`PredicateExpr::any`].

use std::collections::BTreeMap;

use nostos_domain::{PredicateExpr, RULES_VERSION};
use nostos_domain::{Principal, RulesError, ScopeExpr, SyncMode, SyncRules};

/// A validated, pre-compiled ruleset. Built once per load; cheap to consult
/// per subscribe (no parsing on the hot path).
#[derive(Debug, Clone)]
pub struct ActiveRuleset {
    mode: SyncMode,
    checksum: u64,
    /// table -> compiled scope. Absent under `All`. Under `Toggles`/`Hand`,
    /// a table missing from this map is not synced at all (unlisted, or
    /// `sync = false`) — see [`Self::decide`].
    scopes: BTreeMap<String, ScopeExpr>,
}

/// The outcome of evaluating one subscribe against the active ruleset.
#[derive(Debug, Clone, PartialEq)]
pub enum RuleDecision {
    /// Subscribe allowed; AND this into the session predicate.
    ///
    /// `PartialEq` only (no `Eq`): wraps [`PredicateExpr`], which cannot
    /// derive `Eq` because `ColumnValue::Float(f64)` has none — the same
    /// precedent `nostos_domain::scope::ScopeExpr` already sets.
    Allow(PredicateExpr),
    /// Table is toggled off / not listed → close the socket with this reason.
    DeniedTable,
    /// A scope claim is missing on this principal → deny (fail closed).
    DeniedClaim(String),
}

/// A raw scope string is "whole table" when absent or blank; anything else
/// must parse under `ScopeExpr`'s v1 grammar. Mirrors
/// `nostos_domain::rules::active_scope_text` (private to that module) —
/// `SyncRules::validate` already proved every active-section scope parses,
/// so this re-parse is expected to always succeed by the time `compile` is
/// called on a validated ruleset.
fn active_scope_text(scope: Option<&str>) -> Option<&str> {
    scope.map(str::trim).filter(|s| !s.is_empty())
}

fn compile_scope(table: &str, raw: Option<&str>) -> Result<ScopeExpr, RulesError> {
    match active_scope_text(raw) {
        None => Ok(ScopeExpr::default()),
        Some(text) => ScopeExpr::parse(text).map_err(|source| RulesError::Scope {
            table: table.to_string(),
            source,
        }),
    }
}

impl ActiveRuleset {
    /// Compile from a validated `SyncRules`. Selects the active section by
    /// mode; the inactive section is dropped, never consulted.
    ///
    /// # Errors
    /// Propagates [`SyncRules::validate`]'s error (unsupported version,
    /// unknown mode, bad scope, duplicate/invalid table name), or a scope
    /// parse failure if `rules` was not already validated.
    pub fn compile(rules: &SyncRules) -> Result<Self, RulesError> {
        rules.validate()?;
        let checksum = rules.checksum();
        let mut scopes = BTreeMap::new();
        match rules.mode {
            SyncMode::All => {}
            SyncMode::Toggles => {
                for rule in &rules.tables {
                    if !rule.sync {
                        continue;
                    }
                    let scope = compile_scope(&rule.table, rule.scope.as_deref())?;
                    scopes.insert(rule.table.clone(), scope);
                }
            }
            SyncMode::Hand => {
                for rule in &rules.hand {
                    let scope = compile_scope(&rule.table, rule.scope.as_deref())?;
                    scopes.insert(rule.table.clone(), scope);
                }
            }
        }
        Ok(Self {
            mode: rules.mode,
            checksum,
            scopes,
        })
    }

    /// The permissive zero-config ruleset (`sync_mode = "all"`), used when no
    /// `nostos_rules.toml` exists.
    #[must_use]
    pub fn all_mode() -> Self {
        let rules = SyncRules {
            version: RULES_VERSION,
            mode: SyncMode::All,
            tables: Vec::new(),
            hand: Vec::new(),
        };
        Self {
            mode: SyncMode::All,
            checksum: rules.checksum(),
            scopes: BTreeMap::new(),
        }
    }

    #[inline]
    #[must_use]
    pub fn mode(&self) -> SyncMode {
        self.mode
    }

    #[inline]
    #[must_use]
    pub fn checksum(&self) -> u64 {
        self.checksum
    }

    /// Decide one subscribe. Under `All`, always `Allow(PredicateExpr::any())`
    /// — the caller still ANDs tenant scoping in (ADR-0011); this function
    /// deliberately knows nothing about tenants.
    #[must_use]
    pub fn decide(&self, table: &str, principal: &Principal) -> RuleDecision {
        if self.mode == SyncMode::All {
            return RuleDecision::Allow(PredicateExpr::any());
        }
        let Some(scope) = self.scopes.get(table) else {
            return RuleDecision::DeniedTable;
        };
        if let Some(expr) = scope.resolve(principal) {
            return RuleDecision::Allow(expr);
        }
        // `resolve` only returns `None` for a missing claim, but doesn't say
        // which one — recover it by walking the scope's referenced claims
        // (sorted) and reporting the first the principal lacks, so the
        // denial is actionable rather than a bare "denied".
        let missing = scope
            .referenced_claims()
            .into_iter()
            .find(|field| principal.claim(field).is_none())
            .unwrap_or("unknown")
            .to_string();
        RuleDecision::DeniedClaim(missing)
    }

    /// Table names the ruleset syncs, for logs and `GET /rules`.
    #[must_use]
    pub fn synced_tables(&self) -> Vec<&str> {
        self.scopes.keys().map(String::as_str).collect()
    }

    /// The canonical scope text for a synced table, for `GET /rules`.
    ///
    /// Reuses [`ScopeExpr::canonical`] — the same rendering the checksum
    /// feeds on (`nostos_domain::rules::canonical_scope`) — so this is
    /// provably safe to disclose: it only ever prints column names,
    /// operators, and `claims.<name>` references, never a claim's value.
    ///
    /// `None` means the table isn't synced (unlisted, toggled off, or the
    /// ruleset is in `All` mode where `scopes` stays empty — see
    /// [`Self::synced_tables`]). A synced whole-table entry with no scope
    /// clause canonicalizes to `Some(String::new())`, not `None`.
    #[must_use]
    pub fn scope_text(&self, table: &str) -> Option<String> {
        self.scopes.get(table).map(ScopeExpr::canonical)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostos_domain::{ColumnValue, HandRule, TableRule};
    use std::collections::BTreeMap as StdBTreeMap;

    fn principal_with(claims: &[(&str, &str)]) -> Principal {
        let extra: StdBTreeMap<String, String> = claims
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        Principal::with_claims("u1", "t1", extra)
    }

    fn table(name: &str, sync: bool, scope: Option<&str>) -> TableRule {
        TableRule {
            table: name.to_string(),
            sync,
            scope: scope.map(str::to_string),
        }
    }

    fn hand(name: &str, scope: Option<&str>) -> HandRule {
        HandRule {
            table: name.to_string(),
            scope: scope.map(str::to_string),
        }
    }

    #[test]
    fn all_mode_allows_any_table() {
        let ruleset = ActiveRuleset::all_mode();
        let p = principal_with(&[]);
        assert_eq!(
            ruleset.decide("anything", &p),
            RuleDecision::Allow(PredicateExpr::any())
        );
    }

    #[test]
    fn toggles_denies_unlisted_table() {
        let rules = SyncRules {
            version: RULES_VERSION,
            mode: SyncMode::Toggles,
            tables: vec![table("tasks", true, None)],
            hand: vec![],
        };
        let ruleset = ActiveRuleset::compile(&rules).unwrap();
        let p = principal_with(&[]);
        assert_eq!(ruleset.decide("notes", &p), RuleDecision::DeniedTable);
    }

    #[test]
    fn toggles_denies_sync_false() {
        let rules = SyncRules {
            version: RULES_VERSION,
            mode: SyncMode::Toggles,
            tables: vec![table("notes", false, None)],
            hand: vec![],
        };
        let ruleset = ActiveRuleset::compile(&rules).unwrap();
        let p = principal_with(&[]);
        assert_eq!(ruleset.decide("notes", &p), RuleDecision::DeniedTable);
    }

    #[test]
    fn toggles_allows_with_compiled_scope() {
        let rules = SyncRules {
            version: RULES_VERSION,
            mode: SyncMode::Toggles,
            tables: vec![table("tasks", true, Some("owner_id = claims.sub"))],
            hand: vec![],
        };
        let ruleset = ActiveRuleset::compile(&rules).unwrap();
        let p = principal_with(&[]); // account_id "u1" (via principal_with)
        assert_eq!(
            ruleset.decide("tasks", &p),
            RuleDecision::Allow(PredicateExpr::eq("owner_id", ColumnValue::text("u1")))
        );
    }

    #[test]
    fn missing_claim_denies() {
        let rules = SyncRules {
            version: RULES_VERSION,
            mode: SyncMode::Toggles,
            tables: vec![table("tasks", true, Some("org_id = claims.org_id"))],
            hand: vec![],
        };
        let ruleset = ActiveRuleset::compile(&rules).unwrap();
        let p = principal_with(&[]); // no org_id claim
        let decision = ruleset.decide("tasks", &p);
        assert_eq!(decision, RuleDecision::DeniedClaim("org_id".to_string()));
        assert!(!matches!(decision, RuleDecision::Allow(_)));
    }

    #[test]
    fn hand_mode_uses_hand_section_only() {
        let rules = SyncRules {
            version: RULES_VERSION,
            mode: SyncMode::Hand,
            tables: vec![table("notes", true, None)],
            hand: vec![hand("tasks", None)],
        };
        let ruleset = ActiveRuleset::compile(&rules).unwrap();
        let p = principal_with(&[]);
        assert_eq!(ruleset.decide("notes", &p), RuleDecision::DeniedTable);
        assert_eq!(
            ruleset.decide("tasks", &p),
            RuleDecision::Allow(PredicateExpr::any())
        );
    }

    #[test]
    fn toggles_mode_ignores_hand_section() {
        let rules = SyncRules {
            version: RULES_VERSION,
            mode: SyncMode::Toggles,
            tables: vec![table("tasks", true, None)],
            hand: vec![hand("notes", None)],
        };
        let ruleset = ActiveRuleset::compile(&rules).unwrap();
        let p = principal_with(&[]);
        assert_eq!(
            ruleset.decide("tasks", &p),
            RuleDecision::Allow(PredicateExpr::any())
        );
        assert_eq!(ruleset.decide("notes", &p), RuleDecision::DeniedTable);
    }

    #[test]
    fn checksum_matches_domain() {
        let rules = SyncRules {
            version: RULES_VERSION,
            mode: SyncMode::Toggles,
            tables: vec![table("tasks", true, None)],
            hand: vec![],
        };
        let ruleset = ActiveRuleset::compile(&rules).unwrap();
        assert_eq!(ruleset.checksum(), rules.checksum());
    }

    #[test]
    fn scope_text_returns_canonical_rendering_for_synced_table() {
        let rules = SyncRules {
            version: RULES_VERSION,
            mode: SyncMode::Toggles,
            tables: vec![table("tasks", true, Some("owner_id = claims.sub"))],
            hand: vec![],
        };
        let ruleset = ActiveRuleset::compile(&rules).unwrap();
        assert_eq!(
            ruleset.scope_text("tasks"),
            Some("owner_id = claims.sub".to_string())
        );
    }

    #[test]
    fn scope_text_is_empty_string_for_whole_table_entry() {
        let rules = SyncRules {
            version: RULES_VERSION,
            mode: SyncMode::Toggles,
            tables: vec![table("notes", true, None)],
            hand: vec![],
        };
        let ruleset = ActiveRuleset::compile(&rules).unwrap();
        assert_eq!(ruleset.scope_text("notes"), Some(String::new()));
    }

    #[test]
    fn scope_text_is_none_for_unsynced_table() {
        let rules = SyncRules {
            version: RULES_VERSION,
            mode: SyncMode::Toggles,
            tables: vec![table("tasks", true, None)],
            hand: vec![],
        };
        let ruleset = ActiveRuleset::compile(&rules).unwrap();
        assert_eq!(ruleset.scope_text("nonexistent"), None);
    }

    #[test]
    fn all_mode_helper_matches_compiled_all_mode() {
        // A `SyncRules` in `All` mode that still carries table entries (the
        // shape `nostos rules init --mode all` writes so the operator has
        // something to look at) must checksum identically to the bare
        // `all_mode()` helper — otherwise switching to `all` would trigger a
        // resync of every connected client for no behavioral reason, exactly
        // what Task 4's "active section only" canonicalization exists to
        // prevent.
        let rules = SyncRules {
            version: RULES_VERSION,
            mode: SyncMode::All,
            tables: vec![table("tasks", true, Some("a = 1"))],
            hand: vec![hand("notes", None)],
        };
        let compiled = ActiveRuleset::compile(&rules).unwrap();
        assert_eq!(ActiveRuleset::all_mode().checksum(), compiled.checksum());
    }
}
