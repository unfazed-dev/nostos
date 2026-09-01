//! Resource bounds on the client-supplied predicate parser (`where_sql`).
//!
//! `where_sql` arrives from the wire, so every input here is attacker-controlled
//! and — under the OSS default `NOSTOS_SYNC_AUTH=none` — needs no credential at
//! all. These are the two shapes that cost the *server*, not the sender:
//!
//! 1. **Deep nesting** — the parser is recursive descent, so nesting depth is
//!    stack depth. Before `MAX_DEPTH`, 50k `NOT ` tokens (200 KB, comfortably
//!    inside the 16 MiB WS frame ceiling) produced `fatal runtime error: stack
//!    overflow, aborting`. That is a SIGABRT, not a panic: `catch_unwind` does
//!    not contain it and the whole process dies, ending fan-out for every
//!    tenant, not just the sender's session.
//!
//! 2. **Flat width** — `a=1 OR a=1 OR …` is a million nodes at depth 1, so a
//!    depth bound alone would sail straight past it. `PredicateExpr::matches`
//!    walks every node for every replicated event inside the *shared* fan-out
//!    loop, so one client's oversized filter taxes delivery for all of them.
//!
//! Both must be rejected as ordinary parse errors — the connection is refused,
//! the server keeps serving everyone else.

use nostos_domain::{parse_predicate_expr, ParseError};

/// The exact shape that aborted the process before the bound existed.
///
/// If this regresses, the test binary does not fail — it *dies*, and the
/// harness reports a signal rather than an assertion. That is the tell.
#[test]
fn deep_not_chain_is_rejected_not_fatal() {
    for n in [100usize, 1_000, 50_000, 200_000] {
        let src = format!("{}col = 1", "NOT ".repeat(n));
        let err = parse_predicate_expr(&src)
            .expect_err(&format!("depth {n} must be rejected, not parsed"));
        assert!(
            matches!(
                err,
                ParseError::TooDeep { .. } | ParseError::TooLarge { .. }
            ),
            "depth {n} rejected for the wrong reason: {err:?}"
        );
    }
}

/// Nested parens are the other route into the same recursion.
#[test]
fn deep_paren_nesting_is_rejected_not_fatal() {
    let n = 100_000;
    let src = format!("{}col = 1{}", "(".repeat(n), ")".repeat(n));
    let err = parse_predicate_expr(&src).expect_err("deep parens must be rejected");
    assert!(
        matches!(
            err,
            ParseError::TooDeep { .. } | ParseError::TooLarge { .. }
        ),
        "deep parens rejected for the wrong reason: {err:?}"
    );
}

/// Flat-but-huge: depth 1, ~4M nodes. A depth bound alone does not catch this.
#[test]
fn wide_flat_disjunction_is_rejected() {
    let src = std::iter::repeat_n("col = 1", 500_000)
        .collect::<Vec<_>>()
        .join(" OR ");
    let err = parse_predicate_expr(&src).expect_err("a 500k-term OR must be rejected");
    assert!(
        matches!(err, ParseError::TooLarge { .. }),
        "wide disjunction rejected for the wrong reason: {err:?}"
    );
}

/// The bounds must not break predicates a real client would send. Nesting and
/// width here are generous for a subscription filter and must still parse.
#[test]
fn realistic_predicates_still_parse() {
    for src in [
        "tenant_id = 'acme'",
        "status = 'open' AND (priority > 3 OR assignee = 'me')",
        "NOT (archived = 1)",
        "NOT NOT NOT NOT NOT NOT NOT NOT archived = 1",
        "a = 1 OR b = 2 OR c = 3 OR d = 4 OR e = 5 OR f = 6 OR g = 7 OR h = 8",
        "((((((a = 1))))))",
    ] {
        parse_predicate_expr(src)
            .unwrap_or_else(|e| panic!("legit predicate {src:?} rejected: {e:?}"));
    }
}
