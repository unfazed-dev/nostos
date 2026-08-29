//! Shared strict identifier validation (ADR-0013 discipline).
//!
//! The third caller of the table/column identifier defense (write_back.rs's
//! `pg` submodule, snapshot_source.rs, and now the mirror ingest adapter)
//! triggered the lift the snapshot module's note asked for: one regex, one
//! validator, no per-port copies to drift.

use std::sync::OnceLock;

/// The strict identifier regex: a bare lowercase SQL identifier
/// (`^[a-z_][a-z0-9_]*$`). Applied to every client-controlled identifier
/// before any SQL is built — the structural injection defense.
fn ident_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"^[a-z_][a-z0-9_]*$")
            .expect("identifier regex is a valid static pattern")
    })
}

/// Validate one identifier against the strict regex. Returns `Ok(())` if it
/// matches, or `Err` with the offending identifier so the caller can wrap it
/// in its port's error type (`SnapshotError::InvalidTable`,
/// `WriteBackError::InvalidPayload`, …).
pub(crate) fn validate_ident(name: &str) -> Result<(), String> {
    if ident_regex().is_match(name) {
        Ok(())
    } else {
        Err(name.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::validate_ident;

    #[test]
    fn accepts_bare_lowercase_and_rejects_the_rest() {
        assert!(validate_ident("tasks").is_ok());
        assert!(validate_ident("org_id").is_ok());
        assert!(validate_ident("_private").is_ok());
        assert!(validate_ident("col2").is_ok());
        assert!(validate_ident("a; DROP TABLE x").is_err());
        assert!(validate_ident("col\"--").is_err());
        assert!(validate_ident("Title").is_err());
        assert!(validate_ident("col name").is_err());
        assert!(validate_ident("schema.table").is_err());
        assert!(validate_ident("1col").is_err());
        assert!(validate_ident("").is_err());
        assert!(validate_ident("café").is_err());
    }
}
