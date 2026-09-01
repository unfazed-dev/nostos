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
//! - **Slice 2 (shipped):** typed values (`Number`/`Float`/`Bool`) and ordered
//!   comparisons (`Lt`/`Gt`/`Le`/`Ge`). The moat is now complete: ranges over
//!   real decoded values, proven against real PG rows via the JSON payload
//!   extractor in `nostos-infra`.
//! - **Deferred:** parameter-set-digest indexing (the table index already prunes
//!   to O(sessions-on-table)), and the safe-SQL-subset compiler at the subscribe
//!   wire boundary.

use serde::{Deserialize, Serialize};

/// A single column-equality filter: `column <op> value`.
///
/// Carries the column name + the value to compare against. The operator (`=` vs
/// `!=`) is selected by which [`PredicateExpr`] variant wraps it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PredicateFilter {
    pub column: String,
    pub value: ColumnValue,
}

/// A value that can appear in a predicate filter or a row tuple.
///
/// Slice 2 adds the typed variants `Number`/`Float`/`Bool`. Rows decoded from
/// the wire/payload typically arrive as [`ColumnValue::Text`] (the JSON payload
/// quotes every value); ordered leaves (`Lt`/`Gt`/...) coerce a `Text` row value
/// into the filter's type at match time, failing (no match) when it won't parse.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ColumnValue {
    Text(String),
    Number(i64),
    Float(f64),
    Bool(bool),
    /// A `:name` placeholder in a server-side sync-stream template (P5 —
    /// docs/plans/p5-sync-streams-design.md, Decision 2). Produced only by
    /// `predicate_compile`'s parser in literal position and replaced with a
    /// typed value by `bind_params` before the predicate ever evaluates. If
    /// one survives unbound to match time it must NEVER match — a placeholder
    /// never silently over-delivers.
    Param(String),
    /// Sentinel for "any value" — used as a wildcard in filters.
    Any,
}

impl ColumnValue {
    #[inline]
    #[must_use]
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text(s.into())
    }

    #[inline]
    #[must_use]
    pub fn number(n: i64) -> Self {
        Self::Number(n)
    }

    #[inline]
    #[must_use]
    pub fn float(n: f64) -> Self {
        Self::Float(n)
    }

    #[inline]
    #[must_use]
    pub fn boolean(b: bool) -> Self {
        Self::Bool(b)
    }

    /// A `:name` placeholder marker (P5 sync streams). Bound to a concrete
    /// typed value by `predicate_compile::bind_params` before match time.
    #[inline]
    #[must_use]
    pub fn param(name: impl Into<String>) -> Self {
        Self::Param(name.into())
    }
}

/// The boolean expression tree a [`Predicate`] evaluates (ADR-0012 slice 1).
///
/// Leaves are comparisons (`Eq`/`Ne`/`Lt`/`Gt`/`Le`/`Ge`) over a
/// [`PredicateFilter`] (column + value). Combinators are `And`/`Or`/`Not`.
/// `Any` is the match-all leaf — the root of a "give me everything on this
/// table" subscription.
///
/// The matcher takes a caller-supplied `extract` closure that lifts a column
/// value out of the row's tuple image, keeping the domain decoupled from any
/// specific payload encoding.
///
/// # Ordered comparison & type coercion
///
/// Ordered leaves (`Lt`/`Gt`/`Le`/`Ge`) are **typed on the filter side**. A row
/// value arriving as [`ColumnValue::Text`] (the usual case — the JSON payload
/// quotes every value) is parsed into the filter's type at match time; if it
/// won't parse, the leaf does not match (defensive — never over-deliver). See
/// [`PredicateExpr::matches`] and [`cmp_op`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PredicateExpr {
    /// Match-all leaf. The root of an unfiltered subscription.
    Any,
    /// `column = value`.
    Eq(PredicateFilter),
    /// `column != value`.
    Ne(PredicateFilter),
    /// `column < value` (typed — the row value is coerced to the filter type).
    Lt(PredicateFilter),
    /// `column > value`.
    Gt(PredicateFilter),
    /// `column <= value`.
    Le(PredicateFilter),
    /// `column >= value`.
    Ge(PredicateFilter),
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

    /// A `<` leaf (ordered; row value coerced to the filter's type).
    #[inline]
    #[must_use]
    pub fn lt(column: impl Into<String>, value: ColumnValue) -> Self {
        Self::Lt(PredicateFilter {
            column: column.into(),
            value,
        })
    }

    /// A `>` leaf (ordered; row value coerced to the filter's type).
    #[inline]
    #[must_use]
    pub fn gt(column: impl Into<String>, value: ColumnValue) -> Self {
        Self::Gt(PredicateFilter {
            column: column.into(),
            value,
        })
    }

    /// A `<=` leaf (ordered; row value coerced to the filter's type).
    #[inline]
    #[must_use]
    pub fn le(column: impl Into<String>, value: ColumnValue) -> Self {
        Self::Le(PredicateFilter {
            column: column.into(),
            value,
        })
    }

    /// A `>=` leaf (ordered; row value coerced to the filter's type).
    #[inline]
    #[must_use]
    pub fn ge(column: impl Into<String>, value: ColumnValue) -> Self {
        Self::Ge(PredicateFilter {
            column: column.into(),
            value,
        })
    }

    /// Combine two expressions with AND.
    ///
    /// `Any` is the identity for AND, and is COLLAPSED rather than wrapped —
    /// the same normalization [`Predicate::and_eq`] already does one screen
    /// down. This is not cosmetic. `Any` is a match-all *marker*, not a
    /// compilable comparison: the SQL compiler
    /// (`snapshot_source::compile_expr`) deliberately REFUSES it, because a
    /// hand-built `Any` reaching SQL is a widened snapshot. The zero-config
    /// `all` sync mode hands back exactly `Allow(PredicateExpr::any())`
    /// (`transport.rs`), so without this collapse, ANDing a rules scope onto a
    /// snapshot template yields `And([Any, ..])` — which compiles to an error,
    /// which the transport swallows into live-fan-out-only, which silently
    /// starves the client's first sync. Semantics are unchanged for
    /// `matches()`: `Any AND x ≡ x`.
    #[inline]
    #[must_use]
    pub fn and(self, other: PredicateExpr) -> Self {
        match (self, other) {
            (Self::Any, keep) | (keep, Self::Any) => keep,
            (a, b) => Self::And(vec![a, b]),
        }
    }

    /// Combine two expressions with OR.
    ///
    /// `Any` ABSORBS under OR (`Any OR x ≡ Any`) — the dual of [`Self::and`]
    /// above, and the same rule [`Predicate::or_eq`] already applies. Widening
    /// a disjunction back to match-all is the honest result here; it is the
    /// caller's job not to OR a scope against `Any` and expect narrowing.
    #[inline]
    #[must_use]
    pub fn or(self, other: PredicateExpr) -> Self {
        match (self, other) {
            (Self::Any, _) | (_, Self::Any) => Self::Any,
            (a, b) => Self::Or(vec![a, b]),
        }
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
            Self::Lt(f) => matches_ordered(f, extract, Ordering::is_lt),
            Self::Gt(f) => matches_ordered(f, extract, Ordering::is_gt),
            Self::Le(f) => matches_ordered(f, extract, Ordering::is_le),
            Self::Ge(f) => matches_ordered(f, extract, Ordering::is_ge),
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

    /// A predicate matching `table` where `column < value` (ordered; the row
    /// value is coerced to the filter's type — see [`PredicateExpr::lt`]).
    #[inline]
    #[must_use]
    pub fn lt(table: impl Into<String>, column: impl Into<String>, value: ColumnValue) -> Self {
        Self {
            table: table.into(),
            expr: PredicateExpr::lt(column, value),
        }
    }

    /// A predicate matching `table` where `column > value` (ordered).
    #[inline]
    #[must_use]
    pub fn gt(table: impl Into<String>, column: impl Into<String>, value: ColumnValue) -> Self {
        Self {
            table: table.into(),
            expr: PredicateExpr::gt(column, value),
        }
    }

    /// A predicate matching `table` where `column <= value` (ordered).
    #[inline]
    #[must_use]
    pub fn le(table: impl Into<String>, column: impl Into<String>, value: ColumnValue) -> Self {
        Self {
            table: table.into(),
            expr: PredicateExpr::le(column, value),
        }
    }

    /// A predicate matching `table` where `column >= value` (ordered).
    #[inline]
    #[must_use]
    pub fn ge(table: impl Into<String>, column: impl Into<String>, value: ColumnValue) -> Self {
        Self {
            table: table.into(),
            expr: PredicateExpr::ge(column, value),
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
    // An unbound `Param` placeholder must never match (P5 sync streams,
    // docs/plans/p5-sync-streams-design.md Decision 2). Without this guard the
    // `!matches_value(...)` below would invert the placeholder's non-match into
    // a match-EVERYTHING — the exact over-delivery the marker exists to prevent.
    if matches!(f.value, ColumnValue::Param(_)) {
        return false;
    }
    match extract(&f.column) {
        Some(actual) => !matches_value(&f.value, &actual),
        None => false,
    }
}

/// Compare a filter value against a row value for equality/inequality leaves.
///
/// `Any` (as a filter) matches everything. Like types compare by value. A typed
/// filter (`Number`/`Float`/`Bool`) coerces a `Text` row the same way the
/// ordered leaves do, so `eq(c, Bool(true))` matches a row `Text("true")`. A
/// coercion that fails (unparseable text, or a cross-type numeric divide) is a
/// non-match.
//
// Float equality is intentional: the row value is parsed from a deterministic
// text form (e.g. "1.5" → 1.5) and compared against an exact filter value — no
// arithmetic accumulation, so an epsilon margin would be incorrect (1.5 must
// equal 1.5 exactly). The `float_cmp` lint assumes accumulated error, which
// does not apply here.
#[allow(clippy::float_cmp)]
fn matches_value(filter: &ColumnValue, actual: &ColumnValue) -> bool {
    // match_same_arms: the `Param` arm is spelled out ON PURPOSE even though
    // the `_` fallthrough returns the same `false` — an unbound placeholder
    // matching is the cross-tenant over-delivery bug P5 Decision 2 exists to
    // prevent, so the non-match is stated explicitly, not implied.
    #[allow(clippy::match_same_arms)]
    match (filter, actual) {
        (ColumnValue::Any, _) => true,
        // An unbound `Param` placeholder never matches anything (P5 sync
        // streams, Decision 2) — binding replaces it before match time; if one
        // survives, it is a non-match, never a wildcard.
        (ColumnValue::Param(_), _) => false,
        (ColumnValue::Text(a), ColumnValue::Text(b)) => a == b,
        (ColumnValue::Number(a), ColumnValue::Number(b)) => a == b,
        (ColumnValue::Bool(a), ColumnValue::Bool(b)) => a == b,
        // Float equality on coerced/exact float values. f64 == is the honest
        // semantics here: a row Text("1.5") parses to exactly 1.5 and equals a
        // Float(1.5) filter. NaN filters never match (NaN != NaN).
        (ColumnValue::Float(a), ColumnValue::Float(b)) => a == b,
        // Coerce a Text row value into the filter's type, then compare.
        (ColumnValue::Number(a), ColumnValue::Text(s)) => s.parse::<i64>().ok().as_ref() == Some(a),
        (ColumnValue::Bool(a), ColumnValue::Text(s)) => parse_bool(s).as_ref() == Some(a),
        (ColumnValue::Float(a), ColumnValue::Text(s)) => {
            s.parse::<f64>().ok().is_some_and(|b| a == &b)
        }
        _ => false,
    }
}

// --- ordered leaves (Lt/Gt/Le/Ge) -----------------------------------------

/// The three-way ordering of two typed values, or `None` when they are not
/// comparable (different types, or a row value that won't coerce to the filter
/// type). `None` ⇒ the ordered leaf does not match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ordering {
    Less,
    Equal,
    Greater,
}

impl Ordering {
    #[inline]
    fn is_lt(self) -> bool {
        matches!(self, Self::Less)
    }
    #[inline]
    fn is_gt(self) -> bool {
        matches!(self, Self::Greater)
    }
    #[inline]
    fn is_le(self) -> bool {
        matches!(self, Self::Less | Self::Equal)
    }
    #[inline]
    fn is_ge(self) -> bool {
        matches!(self, Self::Greater | Self::Equal)
    }
}

/// Evaluate an ordered leaf: extract the row value, coerce it to the filter's
/// type, compare, and apply the predicate (Lt/Gt/Le/Ge). Absent column or
/// non-coercible value ⇒ `false` (defensive — never over-deliver).
fn matches_ordered(
    f: &PredicateFilter,
    extract: &dyn Fn(&str) -> Option<ColumnValue>,
    keeps: impl Fn(Ordering) -> bool,
) -> bool {
    let Some(actual) = extract(&f.column) else {
        return false;
    };
    match cmp_op(&f.value, &actual) {
        Some(o) => keeps(o),
        None => false,
    }
}

/// Typed comparison of a filter value against a row value for ordered leaves.
/// Typed comparison for ordered leaves, returning the **row value's position
/// relative to the filter**: `Less` ⇔ row < filter (so an `Lt` leaf matches),
/// `Greater` ⇔ row > filter (so `Gt` matches), `Equal` ⇔ row == filter (`Le`/
/// `Ge` match).
///
/// The row value is coerced to the filter's type: a `Text("5")` row is parsed
/// to `Number`/`Float`/`Bool` to match the filter's variant. Cross-type
/// mismatch (filter `Number`, row is typed `Float`, etc.) or a parse failure
/// returns `None` (the leaf does not match — never over-deliver).
///
/// Same-type comparisons are total: `Number` vs `Number` is integer compare,
/// `Float` vs `Float` uses `partial_cmp` (NaN ⇒ `None`), `Text` vs `Text` is
/// lexicographic, `Bool` orders `false < true`.
//
// Float comparison is intentional (see `matches_value` above): values come from
// deterministic text parses, so `partial_cmp` is exact — an epsilon margin would
// be wrong.
#[allow(clippy::float_cmp)]
fn cmp_op(filter: &ColumnValue, actual: &ColumnValue) -> Option<Ordering> {
    use std::cmp::Ordering as StdOrd;
    // Compute actual.cmp(filter) — the row value relative to the filter — so
    // the leaf predicates (is_lt/is_gt/...) read naturally.
    let o: StdOrd = match (filter, actual) {
        // Like-with-like (filter already typed, row already typed).
        (ColumnValue::Number(b), ColumnValue::Number(a)) => a.cmp(b),
        (ColumnValue::Float(b), ColumnValue::Float(a)) => a.partial_cmp(b)?,
        (ColumnValue::Bool(b), ColumnValue::Bool(a)) => a.cmp(b),
        (ColumnValue::Text(b), ColumnValue::Text(a)) => a.cmp(b),
        // Coerce a Text row value into the filter's type, then compare
        // row-vs-filter.
        (ColumnValue::Number(b), ColumnValue::Text(s)) => s.parse::<i64>().ok()?.cmp(b),
        (ColumnValue::Float(b), ColumnValue::Text(s)) => s.parse::<f64>().ok()?.partial_cmp(b)?,
        (ColumnValue::Bool(b), ColumnValue::Text(s)) => parse_bool(s)?.cmp(b),
        _ => return None,
    };
    Some(to_ordering(o))
}

/// Map `std::cmp::Ordering` to our [`Ordering`] (kept private so the public API
/// exposes only the four `is_lt`/`is_gt`/... predicates).
#[inline]
fn to_ordering(o: std::cmp::Ordering) -> Ordering {
    match o {
        std::cmp::Ordering::Less => Ordering::Less,
        std::cmp::Ordering::Equal => Ordering::Equal,
        std::cmp::Ordering::Greater => Ordering::Greater,
    }
}

/// Parse a JSON/text bool. Accepts `true`/`false` (case-insensitive).
fn parse_bool(s: &str) -> Option<bool> {
    match s.to_ascii_lowercase().as_str() {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => None,
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

    // ---- slice 2: typed comparison (Lt/Gt/Le/Ge) ----

    #[test]
    fn lt_number_with_text_row_coercion() {
        // The realistic case: the JSON payload carries every value as a string,
        // so the row value is Text("5"). The filter is typed; the row coerces.
        let p = Predicate::lt("tasks", "priority", ColumnValue::number(10));
        assert!(p.matches(row_view(&[("priority", ColumnValue::text("5"))])));
        assert!(!p.matches(row_view(&[("priority", ColumnValue::text("10"))])));
        assert!(!p.matches(row_view(&[("priority", ColumnValue::text("20"))])));
    }

    #[test]
    fn gt_ge_le_boundaries() {
        let gt = Predicate::gt("t", "n", ColumnValue::number(10));
        let ge = Predicate::ge("t", "n", ColumnValue::number(10));
        let le = Predicate::le("t", "n", ColumnValue::number(10));
        // row = 10 → gt fails, ge passes, le passes.
        assert!(!gt.matches(row_view(&[("n", ColumnValue::text("10"))])));
        assert!(ge.matches(row_view(&[("n", ColumnValue::text("10"))])));
        assert!(le.matches(row_view(&[("n", ColumnValue::text("10"))])));
        // row = 11 → gt/ge pass, le fails.
        assert!(gt.matches(row_view(&[("n", ColumnValue::text("11"))])));
        assert!(ge.matches(row_view(&[("n", ColumnValue::text("11"))])));
        assert!(!le.matches(row_view(&[("n", ColumnValue::text("11"))])));
    }

    #[test]
    fn float_comparison() {
        let p = Predicate::gt("t", "score", ColumnValue::float(1.5));
        assert!(p.matches(row_view(&[("score", ColumnValue::text("2.7"))])));
        assert!(!p.matches(row_view(&[("score", ColumnValue::text("1.5"))])));
        assert!(!p.matches(row_view(&[("score", ColumnValue::text("0.9"))])));
    }

    #[test]
    fn bool_orders_false_before_true() {
        // Bool ordering: false < true. A filter `>= true` only matches true.
        let p = Predicate::ge("t", "active", ColumnValue::boolean(true));
        assert!(p.matches(row_view(&[("active", ColumnValue::text("true"))])));
        assert!(!p.matches(row_view(&[("active", ColumnValue::text("false"))])));
        // Accept 0/1 as bool spellings too.
        assert!(p.matches(row_view(&[("active", ColumnValue::text("1"))])));
        assert!(!p.matches(row_view(&[("active", ColumnValue::text("0"))])));
    }

    #[test]
    fn typed_leaf_unparseable_row_does_not_match() {
        // A text value that won't parse to a number ⇒ defensive no-match.
        let p = Predicate::lt("t", "n", ColumnValue::number(10));
        assert!(!p.matches(row_view(&[("n", ColumnValue::text("abc"))])));
    }

    #[test]
    fn typed_leaf_missing_column_does_not_match() {
        let p = Predicate::gt("t", "n", ColumnValue::number(0));
        assert!(!p.matches(|_| None));
    }

    #[test]
    fn cross_type_mismatch_does_not_match() {
        // Filter is Number, row arrives as an already-typed Float — different
        // numeric kind ⇒ None ⇒ no match (don't silently coerce across the
        // int/float divide; the filter declares its expected type).
        let p = Predicate::lt("t", "n", ColumnValue::number(10));
        assert!(!p.matches(row_view(&[("n", ColumnValue::float(5.0))])));
    }

    #[test]
    fn typed_value_roundtrips_through_serde() {
        let tree = PredicateExpr::And(vec![
            PredicateExpr::lt("n", ColumnValue::number(100)),
            PredicateExpr::gt("score", ColumnValue::float(1.5)),
            PredicateExpr::ge("active", ColumnValue::boolean(true)),
        ]);
        let json = serde_json::to_string(&tree).expect("serialize");
        let back: PredicateExpr = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(tree, back);
    }

    #[test]
    fn ordered_inside_or_and_not() {
        // (priority > 5 AND active = true) OR NOT (score < 1.0)
        let tree = PredicateExpr::Or(vec![
            PredicateExpr::And(vec![
                PredicateExpr::gt("priority", ColumnValue::number(5)),
                PredicateExpr::eq("active", ColumnValue::boolean(true)),
            ]),
            !PredicateExpr::lt("score", ColumnValue::float(1.0)),
        ]);
        // priority=9, active=true → first branch matches.
        assert!(tree.matches(row_view(&[
            ("priority", ColumnValue::text("9")),
            ("active", ColumnValue::text("true")),
            ("score", ColumnValue::text("0.5")),
        ])));
        // priority=1, active=false, score=2.0 → first branch fails; second
        // branch: NOT(score<1.0) = NOT(false) = true → matches.
        assert!(tree.matches(row_view(&[
            ("priority", ColumnValue::text("1")),
            ("active", ColumnValue::text("false")),
            ("score", ColumnValue::text("2.0")),
        ])));
        // priority=1, active=false, score=0.5 → first fails; second: NOT(true)
        // = false → no match.
        assert!(!tree.matches(row_view(&[
            ("priority", ColumnValue::text("1")),
            ("active", ColumnValue::text("false")),
            ("score", ColumnValue::text("0.5")),
        ])));
    }

    // ---- P5 sync streams: an unbound `Param` placeholder NEVER matches ----
    // (docs/plans/p5-sync-streams-design.md Decision 2 — these are the
    // security-relevant semantics: a placeholder that survives to match time
    // is a bug, and the safe answer to a bug is non-match, never a wildcard.)

    #[test]
    fn unbound_param_eq_never_matches() {
        let p = Predicate::eq("tasks", "owner", ColumnValue::param("owner"));
        assert!(!p.matches(row_view(&[("owner", ColumnValue::text("u1"))])));
        // Missing column is still defensive non-match.
        assert!(!p.matches(row_view(&[])));
        // Not even a Param-typed row value (which should never exist) matches.
        assert!(!p.matches(row_view(&[("owner", ColumnValue::param("owner"))])));
    }

    #[test]
    fn unbound_param_ne_does_not_invert_into_match_everything() {
        // The matches_filter_ne guard: without it, `!= :owner` would invert
        // the placeholder's non-match into a match-EVERYTHING — cross-tenant
        // over-delivery. Locked down for both differing and equal row values.
        let p = Predicate::ne("tasks", "owner", ColumnValue::param("owner"));
        assert!(!p.matches(row_view(&[("owner", ColumnValue::text("u1"))])));
        assert!(!p.matches(row_view(&[("owner", ColumnValue::text("other"))])));
        assert!(!p.matches(row_view(&[])));
    }

    #[test]
    fn unbound_param_ordered_leaves_never_match() {
        // Lt/Gt/Le/Ge route through cmp_op, whose fallthrough is None → false.
        for p in [
            Predicate::lt("tasks", "priority", ColumnValue::param("min")),
            Predicate::gt("tasks", "priority", ColumnValue::param("min")),
            Predicate::le("tasks", "priority", ColumnValue::param("min")),
            Predicate::ge("tasks", "priority", ColumnValue::param("min")),
        ] {
            assert!(!p.matches(row_view(&[("priority", ColumnValue::number(5))])));
        }
    }

    #[test]
    fn unbound_param_inside_boolean_tree_still_never_delivers() {
        // Or/Not can't rescue a placeholder: false OR match, NOT false.
        let tree = PredicateExpr::Or(vec![
            PredicateExpr::eq("owner", ColumnValue::param("owner")),
            PredicateExpr::eq("status", ColumnValue::text("open")),
        ]);
        // Placeholder branch false; the concrete branch decides.
        assert!(tree.matches(row_view(&[("status", ColumnValue::text("open"))])));
        assert!(!tree.matches(row_view(&[
            ("owner", ColumnValue::text("u1")),
            ("status", ColumnValue::text("archived")),
        ])));
    }
}

#[cfg(test)]
mod any_collapse_tests {
    use super::*;

    /// `Any` is AND's identity and OR's absorbing element, and both are
    /// COLLAPSED rather than wrapped. Not a tidiness rule: `Any` is a
    /// match-all marker the SQL compiler deliberately refuses, so an
    /// `And([Any, ..])` reaching a snapshot query is a hard error that the
    /// transport swallows into live-fan-out-only — a silently starved first
    /// sync. The zero-config `all` sync mode decides `Allow(Any)`, so this is
    /// the DEFAULT path, not an edge case.
    #[test]
    fn any_collapses_under_and_and_absorbs_under_or() {
        let leaf = || PredicateExpr::eq("status", ColumnValue::text("open"));

        // AND: identity, from either side, and never a wrapper.
        assert_eq!(PredicateExpr::Any.and(leaf()), leaf());
        assert_eq!(leaf().and(PredicateExpr::Any), leaf());
        assert_eq!(PredicateExpr::Any.and(PredicateExpr::Any), PredicateExpr::Any);

        // OR: absorbing — widening to match-all is the honest result.
        assert_eq!(PredicateExpr::Any.or(leaf()), PredicateExpr::Any);
        assert_eq!(leaf().or(PredicateExpr::Any), PredicateExpr::Any);

        // Two real leaves still compose normally — the collapse must not
        // swallow an actual conjunction.
        assert!(matches!(leaf().and(leaf()), PredicateExpr::And(v) if v.len() == 2));
        assert!(matches!(leaf().or(leaf()), PredicateExpr::Or(v) if v.len() == 2));
    }

    /// The collapse is semantics-preserving for row matching: whatever the
    /// tree shape, `Any AND x` must accept exactly what `x` accepts.
    #[test]
    fn collapse_does_not_change_which_rows_match() {
        let scoped = PredicateExpr::Any.and(PredicateExpr::eq("status", ColumnValue::text("open")));
        let row = |status: &'static str| {
            move |col: &str| -> Option<ColumnValue> {
                (col == "status").then(|| ColumnValue::text(status))
            }
        };
        assert!(scoped.matches(row("open")));
        assert!(!scoped.matches(row("archived")));
    }
}
