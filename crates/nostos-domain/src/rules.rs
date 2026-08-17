//! The `nostos_rules.toml` shape: parsed-but-not-compiled sync rules, their
//! canonical text form, and the checksum both the server and the CLI compute
//! from it (ADR-0031).
//!
//! This module owns *data*, not parsing: TOML deserialization into
//! [`SyncRules`] happens above domain (CLI/infra), matching the crate's
//! zero-I/O rule. What lives here is the shape, structural + scope
//! validation, and a canonicalization that two independently-loaded copies
//! of the same rules file must agree on byte-for-byte, so `Subscribe`'s
//! `rules_checksum` field (ADR-0031, D2) can compare a client's checksum
//! against the server's without transmitting the whole file.
//!
//! ## Why "active section only"
//!
//! A `nostos_rules.toml` carries both a `[tables.*]` section (for `toggles`
//! mode) and a `[[rules]]` section (for `hand` mode) so an operator can
//! switch modes without deleting the other section's config. Only the
//! section matching the current `mode` affects behavior, so only it may
//! affect the checksum — otherwise editing a `toggles` table entry while
//! running in `hand` mode would trigger a resync of every hand-authored
//! predicate for no behavioral reason.

use crate::fnv::fnv1a_64;
use crate::scope::{ScopeError, ScopeExpr};
use std::collections::HashSet;
use std::fmt::Write as _;

/// The rules-file format version this server understands. Bumped whenever
/// the on-disk shape changes in a way [`SyncRules::validate`] must reject.
pub const RULES_VERSION: u32 = 1;

/// Which section of `nostos_rules.toml` is authoritative.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SyncMode {
    /// Sync every table, unscoped (still tenant-scoped by the session's
    /// principal — see [`crate::principal`]). The default: a fresh project
    /// syncs everything until an operator opts into narrower rules.
    #[default]
    All,
    /// `[tables.<name>]` entries (generator-owned) are authoritative.
    Toggles,
    /// `[[rules]]` entries (hand-authored) are authoritative.
    Hand,
}

impl SyncMode {
    /// `"all" | "toggles" | "hand"` (lowercase, the on-disk spelling).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            SyncMode::All => "all",
            SyncMode::Toggles => "toggles",
            SyncMode::Hand => "hand",
        }
    }

    /// Parses the on-disk spelling. Strict lowercase match — `sync_mode` in
    /// the TOML file is not case-folded, so a typo becomes a loud
    /// `RulesError::UnknownMode` at the config-parsing boundary rather than
    /// a silent match.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "all" => Some(SyncMode::All),
            "toggles" => Some(SyncMode::Toggles),
            "hand" => Some(SyncMode::Hand),
            _ => None,
        }
    }
}

/// One generator-owned table entry (`[tables.<name>]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRule {
    pub table: String,
    pub sync: bool,
    /// Raw scope text; `None` or empty = whole table (still tenant-scoped).
    pub scope: Option<String>,
}

/// One hand-authored rule (`[[rules]]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandRule {
    pub table: String,
    pub scope: Option<String>,
}

/// One server-defined sync stream (`[streams.<name>]`, P5 sync streams —
/// docs/plans/p5-sync-streams-design.md Decision 1). Streams are
/// server-defined and client-parameterized: `template` is the ADR-0012
/// safe-subset grammar extended with `:param` placeholders in literal
/// position (parsed ONCE at startup; clients supply only values).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamRule {
    pub name: String,
    pub table: String,
    /// Raw predicate template text, e.g. `owner_id = :owner AND priority >= :min`.
    pub template: String,
}

/// The whole `nostos_rules.toml`, parsed but not yet compiled.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SyncRules {
    pub version: u32,
    pub mode: SyncMode,
    pub tables: Vec<TableRule>,
    pub hand: Vec<HandRule>,
    /// Server-defined sync streams (P5). Mode-independent: unlike
    /// `tables`/`hand`, this section is active under EVERY `sync_mode`, so it
    /// is always validated and always participates in the checksum (a streams
    /// edit = a rules edit → resnapshot).
    pub streams: Vec<StreamRule>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RulesError {
    #[error("unsupported rules version {0} (this server understands {RULES_VERSION})")]
    UnsupportedVersion(u32),
    #[error("unknown sync_mode `{0}` (expected all|toggles|hand)")]
    UnknownMode(String),
    #[error("table `{table}`: {source}")]
    Scope { table: String, source: ScopeError },
    #[error("duplicate table `{0}`")]
    DuplicateTable(String),
    #[error(
        "table name `{0}` contains a control character (tab, newline, or similar); \
         `canonical()`'s line format uses these as delimiters, so such a name is \
         indistinguishable from a different rule set with an ordinary name"
    )]
    InvalidTableName(String),
    #[error("duplicate stream `{0}`")]
    DuplicateStream(String),
    #[error(
        "stream name `{0}` contains a control character (tab, newline, or similar); \
         `canonical()`'s line format uses these as delimiters"
    )]
    InvalidStreamName(String),
    #[error("stream `{name}`: template parse error: {source}")]
    StreamTemplate {
        name: String,
        source: crate::predicate_compile::ParseError,
    },
}

/// `canonical()` joins rows as tab-separated fields, one row per newline.
/// A table name that itself contains a tab/newline/other control byte (`<
/// 0x20`) is not distinguishable from those delimiters, so two structurally
/// different `SyncRules` could canonicalize — and therefore checksum — to
/// the same bytes. `validate()` rejects such names outright rather than
/// escaping them, since real Postgres identifiers never contain control
/// characters and only a hand-authored `[[rules]]`/`[tables.*]` entry could
/// produce one.
fn table_name_is_valid(name: &str) -> bool {
    !name.chars().any(char::is_control)
}

/// A raw scope string is "whole table" when absent or blank; anything else
/// must parse under [`ScopeExpr`]'s v1 grammar.
fn active_scope_text(scope: Option<&str>) -> Option<&str> {
    scope.map(str::trim).filter(|s| !s.is_empty())
}

impl SyncRules {
    /// Structural validation + every active-section scope parses.
    /// Inactive table sections are NOT validated (a stale hand section must
    /// not block `toggles` mode). The `streams` section (P5) is active under
    /// EVERY mode, so its templates are always validated — a bad template is
    /// a loud boot error, never a subscribe-time surprise (design Decision 2).
    pub fn validate(&self) -> Result<(), RulesError> {
        if self.version != RULES_VERSION {
            return Err(RulesError::UnsupportedVersion(self.version));
        }
        self.validate_streams()?;
        match self.mode {
            SyncMode::All => Ok(()),
            SyncMode::Toggles => {
                let mut seen = HashSet::with_capacity(self.tables.len());
                for rule in &self.tables {
                    if !table_name_is_valid(&rule.table) {
                        return Err(RulesError::InvalidTableName(rule.table.clone()));
                    }
                    if !seen.insert(rule.table.as_str()) {
                        return Err(RulesError::DuplicateTable(rule.table.clone()));
                    }
                    if let Some(scope) = active_scope_text(rule.scope.as_deref()) {
                        ScopeExpr::parse(scope).map_err(|source| RulesError::Scope {
                            table: rule.table.clone(),
                            source,
                        })?;
                    }
                }
                Ok(())
            }
            SyncMode::Hand => {
                let mut seen = HashSet::with_capacity(self.hand.len());
                for rule in &self.hand {
                    if !table_name_is_valid(&rule.table) {
                        return Err(RulesError::InvalidTableName(rule.table.clone()));
                    }
                    if !seen.insert(rule.table.as_str()) {
                        return Err(RulesError::DuplicateTable(rule.table.clone()));
                    }
                    if let Some(scope) = active_scope_text(rule.scope.as_deref()) {
                        ScopeExpr::parse(scope).map_err(|source| RulesError::Scope {
                            table: rule.table.clone(),
                            source,
                        })?;
                    }
                }
                Ok(())
            }
        }
    }

    /// Streams validation (mode-independent): names/tables free of control
    /// characters, no duplicate stream names, every template parses under the
    /// ADR-0012 grammar + `:param` extension. The grammar itself rejects
    /// JOIN/CTE/subquery shapes and misplaced placeholders — there is no
    /// separate keyword blacklist to drift out of date.
    fn validate_streams(&self) -> Result<(), RulesError> {
        let mut seen = HashSet::with_capacity(self.streams.len());
        for stream in &self.streams {
            if !table_name_is_valid(&stream.name) {
                return Err(RulesError::InvalidStreamName(stream.name.clone()));
            }
            if !seen.insert(stream.name.as_str()) {
                return Err(RulesError::DuplicateStream(stream.name.clone()));
            }
            if !table_name_is_valid(&stream.table) {
                return Err(RulesError::InvalidTableName(stream.table.clone()));
            }
            crate::predicate_compile::parse_predicate_expr(stream.template.trim()).map_err(
                |source| RulesError::StreamTemplate {
                    name: stream.name.clone(),
                    source,
                },
            )?;
        }
        Ok(())
    }

    /// Canonical text over `(version, mode, ACTIVE table section, streams)`:
    /// tables sorted by name, scopes via `ScopeExpr::canonical()`, one
    /// `table\tsync\tscope` line each. `All` canonicalizes the table sections
    /// to just the mode line, so toggling table entries under `all` does not
    /// resync. Streams (P5) are mode-INDEPENDENT — they always emit one
    /// `stream\tname\ttable\ttemplate` line (sorted by name), so a streams
    /// edit changes the checksum under every mode (design: a streams edit =
    /// a rules edit → resnapshot).
    ///
    /// A scope that fails to parse falls back to its trimmed raw text
    /// rather than panicking — `canonical()` cannot fail, only `validate()`
    /// can. Callers that skip `validate()` first don't get the
    /// whitespace/order-insensitivity guarantee for that one line, but they
    /// don't get a panic either.
    #[must_use]
    pub fn canonical(&self) -> String {
        let mut out = format!("v{}\nmode={}\n", self.version, self.mode.as_str());
        match self.mode {
            SyncMode::All => {}
            SyncMode::Toggles => {
                let mut tables: Vec<&TableRule> = self.tables.iter().collect();
                tables.sort_by(|a, b| a.table.cmp(&b.table));
                for rule in tables {
                    let scope = canonical_scope(rule.scope.as_deref());
                    let _ = writeln!(out, "{}\t{}\t{}", rule.table, rule.sync, scope);
                }
            }
            SyncMode::Hand => {
                let mut hand: Vec<&HandRule> = self.hand.iter().collect();
                hand.sort_by(|a, b| a.table.cmp(&b.table));
                for rule in hand {
                    let scope = canonical_scope(rule.scope.as_deref());
                    let _ = writeln!(out, "{}\t{}", rule.table, scope);
                }
            }
        }
        let mut streams: Vec<&StreamRule> = self.streams.iter().collect();
        streams.sort_by(|a, b| a.name.cmp(&b.name));
        for stream in streams {
            let _ = writeln!(
                out,
                "stream\t{}\t{}\t{}",
                stream.name,
                stream.table,
                canonical_template(&stream.template)
            );
        }
        out
    }

    /// FNV-1a 64 of `canonical()`. Stable across processes and machines.
    #[must_use]
    pub fn checksum(&self) -> u64 {
        fnv1a_64(self.canonical().as_bytes())
    }
}

fn canonical_scope(scope: Option<&str>) -> String {
    match active_scope_text(scope) {
        None => String::new(),
        Some(text) => {
            ScopeExpr::parse(text).map_or_else(|_| text.to_string(), |expr| expr.canonical())
        }
    }
}

/// Canonical form of a stream template (P5): the parsed `PredicateExpr`'s
/// JSON (deterministic field order, control characters escaped) when it
/// parses — so whitespace/ordering noise in the TOML doesn't resync — else
/// the raw text with whitespace collapsed (`canonical()` cannot fail; only
/// `validate()` can). Both forms are single-line and control-char-free, so
/// the canonical line format's tab/newline delimiters stay unambiguous.
fn canonical_template(template: &str) -> String {
    let collapsed: String = template.split_whitespace().collect::<Vec<_>>().join(" ");
    crate::predicate_compile::parse_predicate_expr(&collapsed).map_or(collapsed.clone(), |expr| {
        serde_json::to_string(&expr).unwrap_or(collapsed)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn checksum_ignores_whitespace_and_order() {
        let a = SyncRules {
            version: RULES_VERSION,
            mode: SyncMode::Toggles,
            tables: vec![
                table("b", true, Some("x = 1 AND y = 2")),
                table("a", true, Some("p = 3")),
            ],
            hand: vec![],

            streams: vec![],
        };
        let b = SyncRules {
            version: RULES_VERSION,
            mode: SyncMode::Toggles,
            tables: vec![
                table("a", true, Some("  p = 3  ")),
                table("b", true, Some("y=2   AND   x=1")),
            ],
            hand: vec![],

            streams: vec![],
        };
        assert_eq!(a.checksum(), b.checksum());
    }

    #[test]
    fn checksum_changes_with_mode() {
        let tables = vec![table("t", true, None)];
        let hand_rules = vec![hand("t", None)];

        let all = SyncRules {
            version: RULES_VERSION,
            mode: SyncMode::All,
            tables: tables.clone(),
            hand: hand_rules.clone(),

            streams: vec![],
        };
        let toggles = SyncRules {
            mode: SyncMode::Toggles,
            ..all.clone()
        };
        let hand_mode = SyncRules {
            mode: SyncMode::Hand,
            ..all.clone()
        };

        let checksums = [all.checksum(), toggles.checksum(), hand_mode.checksum()];
        assert_ne!(checksums[0], checksums[1]);
        assert_ne!(checksums[1], checksums[2]);
        assert_ne!(checksums[0], checksums[2]);
    }

    #[test]
    fn all_mode_checksum_ignores_sections() {
        let base = SyncRules {
            version: RULES_VERSION,
            mode: SyncMode::All,
            tables: vec![],
            hand: vec![],

            streams: vec![],
        };
        let with_table = SyncRules {
            tables: vec![table("t", true, Some("a = 1"))],
            ..base.clone()
        };
        assert_eq!(base.checksum(), with_table.checksum());
    }

    #[test]
    fn validate_rejects_bad_scope() {
        let rules = SyncRules {
            version: RULES_VERSION,
            mode: SyncMode::Toggles,
            tables: vec![table("t", true, Some("a = 1 OR b = 2"))],
            hand: vec![],

            streams: vec![],
        };
        assert!(matches!(
            rules.validate(),
            Err(RulesError::Scope { table, .. }) if table == "t"
        ));
    }

    #[test]
    fn validate_ignores_inactive_section() {
        let rules = SyncRules {
            version: RULES_VERSION,
            mode: SyncMode::Toggles,
            tables: vec![table("t", true, None)],
            // Syntactically broken, but hand mode is inactive under Toggles.
            hand: vec![hand("t", Some("a = 1 OR b = 2"))],

            streams: vec![],
        };
        assert_eq!(rules.validate(), Ok(()));
    }

    #[test]
    fn validate_rejects_duplicate_table() {
        let rules = SyncRules {
            version: RULES_VERSION,
            mode: SyncMode::Toggles,
            tables: vec![table("t", true, None), table("t", false, None)],
            hand: vec![],

            streams: vec![],
        };
        assert_eq!(
            rules.validate(),
            Err(RulesError::DuplicateTable("t".to_string()))
        );
    }

    #[test]
    fn validate_rejects_future_version() {
        let rules = SyncRules {
            version: RULES_VERSION + 1,
            mode: SyncMode::All,
            tables: vec![],
            hand: vec![],

            streams: vec![],
        };
        assert_eq!(
            rules.validate(),
            Err(RulesError::UnsupportedVersion(RULES_VERSION + 1))
        );
    }

    #[test]
    fn validate_ok_for_well_formed_rules() {
        let rules = SyncRules {
            version: RULES_VERSION,
            mode: SyncMode::Hand,
            tables: vec![],
            hand: vec![hand("t", Some("org_id = claims.org_id"))],

            streams: vec![],
        };
        assert_eq!(rules.validate(), Ok(()));
    }

    #[test]
    fn sync_mode_round_trips_through_str() {
        for mode in [SyncMode::All, SyncMode::Toggles, SyncMode::Hand] {
            assert_eq!(SyncMode::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(SyncMode::parse("bogus"), None);
    }

    #[test]
    fn validate_rejects_control_characters_in_table_name() {
        // Review finding (Task 4, round 1): a table name embedding the
        // canonical() format's own delimiters (`\t`, `\n`) used to sail
        // through validate() and collide with an unrelated, ordinary config.
        // Regression pair straight from the verdict: Z is two normal tables,
        // X2 is one table whose name IS the delimiter-shaped bytes of Z's
        // canonical row content. Both must no longer both validate.
        let z = SyncRules {
            version: RULES_VERSION,
            mode: SyncMode::Toggles,
            tables: vec![table("a", true, None), table("b", false, None)],
            hand: vec![],

            streams: vec![],
        };
        let x2 = SyncRules {
            version: RULES_VERSION,
            mode: SyncMode::Toggles,
            tables: vec![table("a\ttrue\t\nb", false, None)],
            hand: vec![],

            streams: vec![],
        };

        assert_eq!(z.validate(), Ok(()));
        assert_eq!(
            x2.validate(),
            Err(RulesError::InvalidTableName("a\ttrue\t\nb".to_string()))
        );

        // Also cover a bare newline/carriage-return, and the Hand-mode path.
        let cr_table = SyncRules {
            version: RULES_VERSION,
            mode: SyncMode::Toggles,
            tables: vec![table("t\rable", true, None)],
            hand: vec![],

            streams: vec![],
        };
        assert_eq!(
            cr_table.validate(),
            Err(RulesError::InvalidTableName("t\rable".to_string()))
        );

        let hand_bad = SyncRules {
            version: RULES_VERSION,
            mode: SyncMode::Hand,
            tables: vec![],
            hand: vec![hand("ta\nble", None)],

            streams: vec![],
        };
        assert_eq!(
            hand_bad.validate(),
            Err(RulesError::InvalidTableName("ta\nble".to_string()))
        );
    }

    #[test]
    fn none_and_empty_scope_are_equivalent() {
        let none_scope = SyncRules {
            version: RULES_VERSION,
            mode: SyncMode::Toggles,
            tables: vec![table("t", true, None)],
            hand: vec![],

            streams: vec![],
        };
        let empty_scope = SyncRules {
            tables: vec![table("t", true, Some("   "))],
            ..none_scope.clone()
        };
        assert_eq!(none_scope.checksum(), empty_scope.checksum());
    }
}
