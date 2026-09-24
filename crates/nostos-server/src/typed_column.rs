//! Typed column extraction shared by the pg streaming and B2 mirror paths.

use nostos_domain::ColumnValue;

/// Typed column extraction for the pg streaming path AND the B2 mirror path
/// (ADR-0037 plan 1.4): the payload's JSON scalars keep their type —
/// `{"priority":5}` yields [`ColumnValue::Number`] — instead of the old
/// string-only read, which made numeric/bool columns look absent and let
/// `Ne`/`Not(Eq)` predicates over them match wider than intended. Delegates
/// to the canonical `extract_json_column` mapping (ADR-0019) so streaming
/// predicates and the snapshot path can never drift. Pure JSON — never
/// pg-gated (the mirror arm runs featureless).
pub(crate) fn extract_typed_column(payload: &[u8], col: &str) -> Option<ColumnValue> {
    nostos_infra::replicator::extract_json_column(payload)?(col)
}

/// Typed extraction regression (ADR-0037 plan 1.4): the streaming extractor
/// must preserve JSON scalar types. Pinned here because the old inline
/// `as_str()`-only read made `{"priority":5}` extract as absent, so `Ne`/
/// `Not(Eq)` predicates over numeric columns matched wide and `Eq` under-
/// delivered.
#[cfg(all(test, feature = "pg"))]
mod extract_typed_column_tests {
    use super::extract_typed_column;
    use nostos_domain::{ColumnValue, Predicate};

    #[test]
    fn json_scalars_keep_their_type() {
        let payload = br#"{"org_id":"acme","priority":5,"score":2.5,"active":true}"#;
        assert_eq!(
            extract_typed_column(payload, "org_id"),
            Some(ColumnValue::text("acme"))
        );
        assert_eq!(
            extract_typed_column(payload, "priority"),
            Some(ColumnValue::number(5))
        );
        assert_eq!(
            extract_typed_column(payload, "score"),
            Some(ColumnValue::float(2.5))
        );
        assert_eq!(
            extract_typed_column(payload, "active"),
            Some(ColumnValue::boolean(true))
        );
        assert_eq!(extract_typed_column(payload, "missing"), None);
    }

    #[test]
    fn not_eq_over_numeric_column_no_longer_matches_wide() {
        // Before the fix: `priority` extracted as absent → the inner Eq was
        // false → Not(Eq) matched EVERY row, including priority=5 itself.
        let payload = br#"{"priority":5}"#;
        let not_eq = !Predicate::eq("tasks", "priority", ColumnValue::number(5));
        assert!(!not_eq.matches(|c| extract_typed_column(payload, c)));

        // And the under-delivery side: Ne now sees the value and matches.
        let ne = Predicate::ne("tasks", "priority", ColumnValue::number(7));
        assert!(ne.matches(|c| extract_typed_column(payload, c)));
        let eq = Predicate::eq("tasks", "priority", ColumnValue::number(5));
        assert!(eq.matches(|c| extract_typed_column(payload, c)));
    }
}
