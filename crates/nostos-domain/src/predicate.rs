//! Predicates — the heart of Nostos's "dynamic reactive sync" moat (ADR-0003).
//!
//! A `Predicate` is the *scoped, authorized* subscription filter a client sends
//! when it opens a sync session. The server evaluates each incoming row change
//! against the set of live predicates and delivers only the matches.
//!
//! **This replaces PowerSync's static buckets.** Buckets are cardinality-bound
//! (one per unique filter value, hard-capped at 1,000/user). Predicates are
//! evaluated live and have no fixed ceiling — a user can scroll 100,000 items
//! and sync exactly what they look at.
//!
//! Week-1 scope: `table` + simple `column = value` equality filters — enough
//! to benchmark the fan-out path. The full boolean-tree expression engine
//! arrives in Phase 2.

use serde::{Deserialize, Serialize};

/// A single column-equality filter: `column = value`.
///
/// Week 1 supports only equality. Phase 2 generalizes to a boolean expression
/// tree (`And`, `Or`, `=`, `!=`, `<`, `>`, `IN`, ...).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PredicateFilter {
    pub column: String,
    pub value: ColumnValue,
}

/// A value that can appear in a predicate filter or a row tuple.
///
/// Kept deliberately small for Week 1 (text). Phase 2 adds numbers, bytes,
/// booleans, timestamps with typed comparison.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ColumnValue {
    Text(String),
    /// Sentinel for "any value" — used in wildcard predicates.
    Any,
}

impl ColumnValue {
    #[inline]
    #[must_use]
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text(s.into())
    }
}

/// A client's subscription: "give me changes to `table` matching `filters`".
///
/// Predicates are indexed by `table` in the `SessionStore` so the router can
/// prune the candidate-session set to O(sessions-on-this-table) before
/// evaluating filters — the key to scaling past PowerSync's bucket model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Predicate {
    pub table: String,
    /// Conjunction of equality filters (all must match). Empty = match-all
    /// for this table.
    pub filters: Vec<PredicateFilter>,
}

impl Predicate {
    /// A predicate that matches every change to `table`.
    #[inline]
    #[must_use]
    pub fn all(table: impl Into<String>) -> Self {
        Self {
            table: table.into(),
            filters: Vec::new(),
        }
    }

    /// A predicate matching `table` where `column = value`.
    #[inline]
    #[must_use]
    pub fn eq(table: impl Into<String>, column: impl Into<String>, value: ColumnValue) -> Self {
        Self {
            table: table.into(),
            filters: vec![PredicateFilter {
                column: column.into(),
                value,
            }],
        }
    }

    /// Add an additional equality filter (conjunction).
    #[inline]
    #[must_use]
    pub fn and_eq(mut self, column: impl Into<String>, value: ColumnValue) -> Self {
        self.filters.push(PredicateFilter {
            column: column.into(),
            value,
        });
        self
    }

    /// Does this predicate match the given (table, column→value) view of a row?
    ///
    /// The router calls this after the `table` index has pruned candidates.
    /// `extract` is a caller-supplied closure that lifts a column value out of
    /// the row's tuple image — the domain stays decoupled from any specific
    /// payload encoding by accepting this callback.
    ///
    /// `extract` returns an **owned** `Option<ColumnValue>` rather than a
    /// borrow. This sidesteps a higher-rank-lifetime trap (the closure would
    /// otherwise be unable to return a borrow of data it builds on the fly),
    /// and the clone cost is negligible — filters are a handful of small
    /// values, evaluated only for the candidate sessions the table index
    /// already pruned to.
    ///
    /// Week-1 semantics: all filters must match (AND of equalities). A filter
    /// whose column is absent from the row does **not** match (defensive — we
    /// never silently over-deliver).
    #[inline]
    pub fn matches<F>(&self, extract: F) -> bool
    where
        F: Fn(&str) -> Option<ColumnValue>,
    {
        if self.filters.is_empty() {
            return true; // match-all
        }
        self.filters.iter().all(|f| match extract(&f.column) {
            Some(actual) => matches_value(&f.value, &actual),
            None => false, // column missing → don't match (no over-delivery)
        })
    }
}

/// Compare a filter value against a row value. `Any` (as a filter) matches
/// everything; `Any` as an *actual* row value never arises (only filters use
/// it), so any other combination is a non-match.
fn matches_value(filter: &ColumnValue, actual: &ColumnValue) -> bool {
    match (filter, actual) {
        (ColumnValue::Any, _) => true,
        (ColumnValue::Text(a), ColumnValue::Text(b)) => a == b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a column-view closure that owns its data. Used by the predicate
    /// tests to exercise `Predicate::matches`.
    fn row_view(pairs: &[(&str, ColumnValue)]) -> impl Fn(&str) -> Option<ColumnValue> {
        let owned: Vec<(String, ColumnValue)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect();
        move |col: &str| owned.iter().find(|(k, _)| k == col).map(|(_, v)| v.clone())
    }

    #[test]
    fn match_all_predicate() {
        let p = Predicate::all("tasks");
        assert!(p.matches(|_| None)); // no columns, but match-all still matches
    }

    #[test]
    fn single_equality_match() {
        let p = Predicate::eq("tasks", "org_id", ColumnValue::text("acme"));
        assert!(p.matches(row_view(&[("org_id", ColumnValue::text("acme"))])));
        assert!(!p.matches(row_view(&[("org_id", ColumnValue::text("other"))])));
    }

    #[test]
    fn conjunction_of_filters() {
        let p = Predicate::eq("tasks", "org_id", ColumnValue::text("acme"))
            .and_eq("assignee_id", ColumnValue::text("u1"));
        let both = row_view(&[
            ("org_id", ColumnValue::text("acme")),
            ("assignee_id", ColumnValue::text("u1")),
        ]);
        let partial = row_view(&[("org_id", ColumnValue::text("acme"))]);
        assert!(p.matches(both));
        assert!(!p.matches(partial)); // missing assignee_id → no match
    }

    #[test]
    fn any_value_wildcard() {
        let p = Predicate::eq("tasks", "org_id", ColumnValue::Any);
        assert!(p.matches(row_view(&[("org_id", ColumnValue::text("whatever"))])));
    }

    #[test]
    fn missing_column_does_not_over_deliver() {
        let p = Predicate::eq("tasks", "org_id", ColumnValue::text("acme"));
        // Row has no org_id column at all → must NOT match (defensive).
        assert!(!p.matches(|_| None));
    }
}
