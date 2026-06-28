//! Column extraction for the predicate `matches` seam (ADR-0012 slice 2).
//!
//! [`PgReplicator`](super::pg::PgReplicator) renders each row as a JSON object
//! `{"col":"val",...}` via `tuple_to_json_payload` — every value is a JSON
//! **string**, regardless of its SQL type. [`extract_json_column`] parses that
//! payload **once** at construction into an owned column map and returns a
//! closure the fan-out router calls per candidate predicate. The closure maps
//! each column lookup to a [`ColumnValue`], letting the domain's typed
//! comparison leaves coerce the string at match time.
//!
//! Parsing once (rather than per-`matches` call) keeps the predicate evaluation
//! cost at O(changed_rows × matching_predicates × columns) with a single
//! `serde_json::from_slice` per row — not per (row, predicate) pair.

use nostos_domain::ColumnValue;
use serde_json::Value;

/// Build a column extractor over a JSON-object row payload.
///
/// Parses the payload once into an owned column map and returns a closure
/// `Fn(&str) -> Option<ColumnValue>` suitable for handing to
/// [`nostos_domain::Predicate::matches`] /
/// [`nostos_domain::PredicateExpr::matches`]. The closure owns the parsed data,
/// so it has no lifetime bound.
///
/// Returns `None` when the payload is not a valid JSON object — the router then
/// skips predicate evaluation for that row, so a malformed row is simply not
/// delivered (never over-delivered).
pub fn extract_json_column(payload: &[u8]) -> Option<impl Fn(&str) -> Option<ColumnValue>> {
    let value: Value = serde_json::from_slice(payload).ok()?;
    let object = value.as_object()?;
    // Materialize an owned (String, ColumnValue) map up front so the returned
    // closure doesn't borrow the transient `Value`. This is the single parse
    // per row — per-predicate lookups below are cheap map lookups.
    let cols: Vec<(String, ColumnValue)> = object
        .iter()
        .filter_map(|(k, v)| json_value_to_column_value(v).map(|cv| (k.clone(), cv)))
        .collect();
    Some(move |column: &str| {
        cols.iter()
            .find(|(k, _)| k == column)
            .map(|(_, v)| v.clone())
    })
}

/// Map a `serde_json::Value` (as it appears in the payload) to a
/// [`ColumnValue`]. Our payload quotes every value, so the common case is
/// `Value::String` → `ColumnValue::Text` (the typed leaves coerce on the filter
/// side). The number/bool/null arms are handled defensively in case a future
/// payload format emits typed JSON.
fn json_value_to_column_value(v: &Value) -> Option<ColumnValue> {
    match v {
        Value::String(s) => Some(ColumnValue::text(s)),
        // Defensive: a numeric JSON literal → Number (i64) when it fits, else
        // Float. Unparseable-as-i64 floats still compare via the Float path.
        Value::Number(n) if n.is_i64() => Some(ColumnValue::number(n.as_i64()?)),
        Value::Number(n) => Some(ColumnValue::float(n.as_f64()?)),
        Value::Bool(b) => Some(ColumnValue::boolean(*b)),
        // Null and structured values can't back a comparison leaf — no match.
        Value::Null | Value::Array(_) | Value::Object(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostos_domain::Predicate;

    /// Build a payload in the exact shape `tuple_to_json_payload` emits: a JSON
    /// object with every value quoted.
    fn payload(cols: &[(&str, &str)]) -> Vec<u8> {
        let mut out = String::from('{');
        for (i, (k, v)) in cols.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push('"');
            out.push_str(k);
            out.push_str("\":\"");
            out.push_str(v);
            out.push('"');
        }
        out.push('}');
        out.into_bytes()
    }

    #[test]
    fn extracts_present_columns_as_text() {
        let p = payload(&[("org_id", "acme"), ("priority", "5"), ("active", "true")]);
        let extract = extract_json_column(&p).expect("valid JSON object");
        assert_eq!(extract("org_id"), Some(ColumnValue::text("acme")));
        assert_eq!(extract("priority"), Some(ColumnValue::text("5")));
        assert_eq!(extract("active"), Some(ColumnValue::text("true")));
    }

    #[test]
    fn missing_column_yields_none() {
        let p = payload(&[("org_id", "acme")]);
        let extract = extract_json_column(&p).expect("valid JSON object");
        assert!(extract("nope").is_none());
    }

    #[test]
    fn malformed_payload_yields_none() {
        // Not a JSON object → the builder itself returns None.
        assert!(extract_json_column(b"not json").is_none());
        // A JSON array (not an object) → also None.
        assert!(extract_json_column(b"[1,2,3]").is_none());
    }

    #[test]
    fn end_to_end_typed_predicate_matches_real_payload_shape() {
        // The point of slice 2: a typed predicate (`priority > 3`) matches a row
        // rendered exactly as PgReplicator renders it (all-quoted JSON).
        let row = payload(&[("org_id", "acme"), ("priority", "7")]);
        let extract = extract_json_column(&row).expect("valid JSON object");
        let pred = Predicate::gt("tasks", "priority", ColumnValue::number(3));
        assert!(pred.matches(&extract));

        // A low-priority row does NOT match.
        let row = payload(&[("org_id", "acme"), ("priority", "1")]);
        let extract = extract_json_column(&row).expect("valid JSON object");
        assert!(!pred.matches(&extract));
    }

    #[test]
    fn defensive_typed_json_values() {
        // If a future payload emits typed JSON (not all-quoted), the extractor
        // still maps sensibly. This guards against a silent regression if the
        // payload format changes.
        let payload = br#"{"n":42,"f":2.5,"b":true,"s":"x"}"#;
        let extract = extract_json_column(payload).expect("valid JSON object");
        assert_eq!(extract("n"), Some(ColumnValue::number(42)));
        assert_eq!(extract("f"), Some(ColumnValue::float(2.5)));
        assert_eq!(extract("b"), Some(ColumnValue::boolean(true)));
        assert_eq!(extract("s"), Some(ColumnValue::text("x")));
    }
}
