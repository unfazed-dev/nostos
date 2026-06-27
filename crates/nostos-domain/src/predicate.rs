//! Predicates — the heart of Nostos's "dynamic reactive sync" moat (ADR-0003,
//! ADR-0012).
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
//! ## Slice status (ADR-0012)
//!
//! - **Slice 1 (shipped):** the boolean tree — `And`/`Or`/`Not` over `Eq`/`Ne`
//!   comparison leaves, on the existing text-only [`ColumnValue`]. The matcher
//!   is fully decoupled from the payload codec via the `extract` closure, so it
//!   is testable *without* the pgoutput column decoder.
//! - **Deferred:** typed values (`Number`/`Float`/`Bool`) and ordered
//!   comparisons (`Lt`/`Gt`/`Le`/`Ge`) — these genuinely require the column
//!   decoder to prove real-world correctness. They extend this tree
//!   additively; the recursion and combinator API below are the foundation.

use serde::{Deserialize, Serialize};

/// A single column-equality filter: `column <op> value`.
///
/// Carries the column name + the value to compare against. The operator (`=` vs
/// `!=`) is selected by which [`PredicateExpr`] variant wraps it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PredicateFilter {
    pub column: String,
    pub value: ColumnValue,
}

/// A value that can appear in a predicate filter or a row tuple.
///
/// Slice 1 is text-only (`Text` + the `Any` wildcard). Typed values arrive with
/// the pgoutput column decoder (ADR-0012 follow-up) — adding variants here is
/// trivially additive.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ColumnValue {
    Text(String),
    /// Sentinel for "any value" — used as a wildcard in filters.
    Any,
}

impl ColumnValue {
    #[inline]
    #[must_use]
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text(s.into())
    }
}

/// The boolean expression tree a [`Predicate`] evaluates (ADR-0012 slice 1).
///
/// Leaves are comparisons (`Eq`/`Ne`) over a [`PredicateFilter`] (column +
/// value). Combinators are `And`/`Or`/`Not`. `Any` is the match-all leaf — the
/// root of a "give me everything on this table" subscription.
///
/// The matcher takes a caller-supplied `extract` closure that lifts a column
/// value out of the row's tuple image, keeping the domain decoupled from any
/// specific payload encoding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PredicateExpr {
    /// Match-all leaf. The root of an unfiltered subscription.
    Any,
    /// `column = value`.
    Eq(PredicateFilter),
    /// `column != value`.
    Ne(PredicateFilter),
    /// All children must match (logical AND).
    And(Vec<PredicateExpr>),
    /// At least one child must match (logical OR).
    Or(Vec<PredicateExpr>),
    /// Logical negation of the child.
    Not(Box<PredicateExpr>),
}

impl PredicateExpr {
    /// A match-all leaf.
    #[inline]
    #[must_use]
    pub fn any() -> Self {
        Self::Any
    }

    /// An `=` leaf.
    #[inline]
    #[must_use]
    pub fn eq(column: impl Into<String>, value: ColumnValue) -> Self {
        Self::Eq(PredicateFilter {
            column: column.into(),
            value,
        })
    }

    /// A `!=` leaf — the safe inequality primitive (see module-level note on
    /// missing columns: `Ne` on an absent column does NOT match, never
    /// over-delivering).
    #[inline]
    #[must_use]
    pub fn ne(column: impl Into<String>, value: ColumnValue) -> Self {
        Self::Ne(PredicateFilter {
            column: column.into(),
            value,
        })
    }

    /// Combine two expressions with AND.
    #[inline]
    #[must_use]
    pub fn and(self, other: PredicateExpr) -> Self {
        Self::And(vec![self, other])
    }

    /// Combine two expressions with OR.
    #[inline]
    #[must_use]
    pub fn or(self, other: PredicateExpr) -> Self {
        Self::Or(vec![self, other])
    }

    /// Does this expression match the given (column→value) view of a row?
    ///
    /// `extract` lifts a column value out of the row's tuple image — the domain
    /// stays decoupled from any specific payload encoding by accepting this
    /// callback. It returns an **owned** `Option<ColumnValue>` rather than a
    /// borrow, sidestepping a higher-rank-lifetime trap.
    ///
    /// # Missing-column semantics (two-valued logic)
    ///
    /// A filter whose column is absent from the row does **not** match — for
    /// both `Eq` and `Ne` — because we cannot verify the relation, and Nostos
    /// never silently over-delivers. `And`/`Or`/`Not` then compose with
    /// standard two-valued boolean logic.
    ///
    /// **Documented edge:** because absence makes `Eq` return `false`, `Not(Eq
    /// {absent})` returns `true`. This is the SQL-NULL three-valued-logic gap.
    /// Slice 1 deliberately stays two-valued (no speculative NULL handling);
    /// `Ne` is provided as the safe inequality that never matches on absence.
    /// If real schemas demand it, three-valued logic is a documented future
    /// refinement (ADR-0012).
    #[inline]
    pub fn matches<F>(&self, extract: F) -> bool
    where
        F: Fn(&str) -> Option<ColumnValue>,
    {
        // Delegate to a trait-object recursion so the tree monomorphizes to a
        // single copy. Calling `matches::<&F>` recursively would grow the type
        // one reference per depth (F, &F, &&F, …) and hit the recursion limit.
        self.matches_dyn(&extract)
    }

    fn matches_dyn(&self, extract: &dyn Fn(&str) -> Option<ColumnValue>) -> bool {
        match self {
            Self::Any => true,
            Self::Eq(f) => matches_filter_eq(f, extract),
            Self::Ne(f) => matches_filter_ne(f, extract),
            Self::And(parts) => parts.iter().all(|p| p.matches_dyn(extract)),
            Self::Or(parts) => parts.iter().any(|p| p.matches_dyn(extract)),
            Self::Not(inner) => !inner.matches_dyn(extract),
        }
    }
}

/// `!expr` wraps an expression in a [`PredicateExpr::Not`] node — the
/// idiomatic spelling of negation (a `.not()` method would shadow this trait).
impl std::ops::Not for PredicateExpr {
    type Output = Self;

    #[inline]
    fn not(self) -> Self {
        Self::Not(Box::new(self))
    }
}

/// A client's subscription: "give me changes to `table` matching `expr`".
///
/// `table` indexes the predicate in the `SessionStore` so the router can prune
/// the candidate-session set to O(sessions-on-this-table) before evaluating the
/// expression tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Predicate {
    pub table: String,
    /// The boolean expression evaluated against each row on `table`. Empty
    /// filters (the historical shape) compile to `Any` (match-all).
    pub expr: PredicateExpr,
}

impl Predicate {
    /// A predicate that matches every change to `table`.
    #[inline]
    #[must_use]
    pub fn all(table: impl Into<String>) -> Self {
        Self {
            table: table.into(),
            expr: PredicateExpr::Any,
        }
    }

    /// A predicate matching `table` where `column = value`.
    #[inline]
    #[must_use]
    pub fn eq(table: impl Into<String>, column: impl Into<String>, value: ColumnValue) -> Self {
        Self {
            table: table.into(),
            expr: PredicateExpr::eq(column, value),
        }
    }

    /// A predicate matching `table` where `column != value`.
    #[inline]
    #[must_use]
    pub fn ne(table: impl Into<String>, column: impl Into<String>, value: ColumnValue) -> Self {
        Self {
            table: table.into(),
            expr: PredicateExpr::ne(column, value),
        }
    }

    /// Add an additional equality filter (conjunction). Folds into the existing
    /// expression as an `And`, preserving identical semantics to the historical
    /// `Vec<PredicateFilter>` AND-of-equalities builder.
    #[inline]
    #[must_use]
    pub fn and_eq(mut self, column: impl Into<String>, value: ColumnValue) -> Self {
        self.expr = match self.expr {
            // First filter off a match-all root: collapse Any → Eq (no spurious
            // And(Any, Eq) wrapper — the tree stays minimal).
            PredicateExpr::Any => PredicateExpr::eq(column, value),
            other => other.and(PredicateExpr::eq(column, value)),
        };
        self
    }

    /// Add a disjunctive equality branch: this predicate matches if either the
    /// existing expression OR `column = value` holds.
    #[inline]
    #[must_use]
    pub fn or_eq(mut self, column: impl Into<String>, value: ColumnValue) -> Self {
        self.expr = match self.expr {
            PredicateExpr::Any => PredicateExpr::Any, // (Any OR x) ≡ Any
            other => other.or(PredicateExpr::eq(column, value)),
        };
        self
    }

    /// Does this predicate match the given (column→value) view of a row?
    ///
    /// Delegates to [`PredicateExpr::matches`] — the stable seam every caller
    /// uses. See that method's docs for the missing-column semantics.
    #[inline]
    pub fn matches<F>(&self, extract: F) -> bool
    where
        F: Fn(&str) -> Option<ColumnValue>,
    {
        self.expr.matches(extract)
    }
}

/// `!predicate` negates the predicate's expression — the idiomatic spelling of
/// negation (a `.not()` method would shadow this trait).
impl std::ops::Not for Predicate {
    type Output = Self;

    #[inline]
    fn not(mut self) -> Self {
        self.expr = !self.expr;
        self
    }
}

// --- leaf evaluators (split so the match arms stay readable) ---------------

/// `column = value`: matches iff the column is present and the values agree.
/// Absent column ⇒ `false` (never over-deliver).
#[inline]
fn matches_filter_eq(f: &PredicateFilter, extract: &dyn Fn(&str) -> Option<ColumnValue>) -> bool {
    match extract(&f.column) {
        Some(actual) => matches_value(&f.value, &actual),
        None => false,
    }
}

/// `column != value`: matches iff the column is present and the values differ.
/// Absent column ⇒ `false` — `Ne` is the safe inequality that never
/// over-delivers when the column can't be read (see module docs).
#[inline]
fn matches_filter_ne(f: &PredicateFilter, extract: &dyn Fn(&str) -> Option<ColumnValue>) -> bool {
    match extract(&f.column) {
        Some(actual) => !matches_value(&f.value, &actual),
        None => false,
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
    /// tests to exercise `matches`.
    fn row_view(pairs: &[(&str, ColumnValue)]) -> impl Fn(&str) -> Option<ColumnValue> {
        let owned: Vec<(String, ColumnValue)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect();
        move |col: &str| owned.iter().find(|(k, _)| k == col).map(|(_, v)| v.clone())
    }

    // ---- backward compatibility: the historical builders behave identically ----

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
    fn conjunction_of_filters_via_and_eq() {
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

    // ---- slice 1: inequality, disjunction, negation ----

    #[test]
    fn ne_matches_when_value_differs() {
        let p = Predicate::ne("tasks", "status", ColumnValue::text("archived"));
        assert!(p.matches(row_view(&[("status", ColumnValue::text("open"))])));
        assert!(!p.matches(row_view(&[("status", ColumnValue::text("archived"))])));
    }

    #[test]
    fn ne_missing_column_is_defensive() {
        // The safe inequality: an absent column can't be verified to differ,
        // so Ne does NOT match — never over-deliver.
        let p = Predicate::ne("tasks", "status", ColumnValue::text("archived"));
        assert!(!p.matches(|_| None));
    }

    #[test]
    fn or_eq_matches_either_branch() {
        let p = Predicate::eq("tasks", "status", ColumnValue::text("open"))
            .or_eq("status", ColumnValue::text("in_progress"));
        assert!(p.matches(row_view(&[("status", ColumnValue::text("open"))])));
        assert!(p.matches(row_view(&[("status", ColumnValue::text("in_progress"))])));
        assert!(!p.matches(row_view(&[("status", ColumnValue::text("done"))])));
    }

    #[test]
    fn or_eq_off_match_all_is_still_match_all() {
        // (Any OR x) ≡ Any — folding preserves the wildcard.
        let p = Predicate::all("tasks").or_eq("status", ColumnValue::text("open"));
        assert!(p.matches(row_view(&[("status", ColumnValue::text("done"))])));
        assert!(p.matches(|_| None));
    }

    #[test]
    fn not_inverts_a_match() {
        let p = !Predicate::eq("tasks", "status", ColumnValue::text("archived"));
        assert!(!p.matches(row_view(&[("status", ColumnValue::text("archived"))])));
        assert!(p.matches(row_view(&[("status", ColumnValue::text("open"))])));
    }

    #[test]
    fn not_of_missing_eq_returns_true_pinned_edge() {
        // Documented two-valued edge: Eq{absent} → false, so Not(Eq{absent}) →
        // true. This is the SQL-NULL three-valued-logic gap (ADR-0012). Pinned
        // so the behavior is never surprising; use `Ne` for safe inequality.
        let p = !Predicate::eq("tasks", "status", ColumnValue::text("archived"));
        assert!(p.matches(|_| None));
    }

    // ---- combinator algebra ----

    #[test]
    fn and_eq_collapses_any_to_eq() {
        // and_eq on a match-all root must not wrap Any in And(Any, Eq) — it
        // collapses to a bare Eq, matching the historical Vec<filter> shape.
        let p = Predicate::all("tasks").and_eq("org_id", ColumnValue::text("acme"));
        assert_eq!(
            p.expr,
            PredicateExpr::eq("org_id", ColumnValue::text("acme"))
        );
    }

    #[test]
    fn nested_and_or_not_tree() {
        // (org_id=acme AND (status=open OR status=in_progress)) AND NOT owner=bob
        let p = Predicate::eq("tasks", "org_id", ColumnValue::text("acme"))
            .or_eq("status", ColumnValue::text("open"))
            .and_eq("owner", ColumnValue::text("bob"));
        // The above is a smoke build of the combinators; exercise a real tree:
        let tree = PredicateExpr::And(vec![
            PredicateExpr::eq("org_id", ColumnValue::text("acme")),
            PredicateExpr::Or(vec![
                PredicateExpr::eq("status", ColumnValue::text("open")),
                PredicateExpr::eq("status", ColumnValue::text("in_progress")),
            ]),
            PredicateExpr::Not(Box::new(PredicateExpr::eq(
                "owner",
                ColumnValue::text("bob"),
            ))),
        ]);
        let _ = p; // built without panic
                   // Matches: acme + open, no bob.
        assert!(tree.matches(row_view(&[
            ("org_id", ColumnValue::text("acme")),
            ("status", ColumnValue::text("open")),
            ("owner", ColumnValue::text("alice")),
        ])));
        // Fails: acme but status=done.
        assert!(!tree.matches(row_view(&[
            ("org_id", ColumnValue::text("acme")),
            ("status", ColumnValue::text("done")),
            ("owner", ColumnValue::text("alice")),
        ])));
        // Fails: acme + open, but owner=bob (negation excludes it).
        assert!(!tree.matches(row_view(&[
            ("org_id", ColumnValue::text("acme")),
            ("status", ColumnValue::text("open")),
            ("owner", ColumnValue::text("bob")),
        ])));
        // Fails: other org.
        assert!(!tree.matches(row_view(&[
            ("org_id", ColumnValue::text("other")),
            ("status", ColumnValue::text("open")),
            ("owner", ColumnValue::text("alice")),
        ])));
    }

    #[test]
    fn empty_and_is_true_empty_or_is_false() {
        // Standard vacuous truth: And of nothing is true, Or of nothing is false.
        assert!(PredicateExpr::And(vec![]).matches(|_| None));
        assert!(!PredicateExpr::Or(vec![]).matches(|_| None));
    }

    #[test]
    fn expr_roundtrips_through_serde() {
        // Predicates cross the wire (subscribe frame) and are stored in
        // sessions — the tree must serialize without losing structure.
        let tree = PredicateExpr::And(vec![
            PredicateExpr::eq("org_id", ColumnValue::text("acme")),
            PredicateExpr::Not(Box::new(PredicateExpr::ne(
                "status",
                ColumnValue::text("archived"),
            ))),
            PredicateExpr::Or(vec![
                PredicateExpr::eq("a", ColumnValue::Any),
                PredicateExpr::Any,
            ]),
        ]);
        let json = serde_json::to_string(&tree).expect("serialize");
        let back: PredicateExpr = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(tree, back);
    }
}
