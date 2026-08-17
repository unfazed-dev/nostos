//! Claims-scoped rules grammar (ADR-0031, "Grammar (v1)").
//!
//! `parse_predicate_expr` (`crate::predicate_compile`) parses literals only —
//! it has no notion of `claims.<field>`. A [`ScopeExpr`] is the small
//! superset a rules-config `[[rules]]` entry authors: a template that is
//! parsed once at config load (`ScopeExpr::parse`) and resolved once per
//! session (`ScopeExpr::resolve`) against the connecting [`Principal`]'s
//! claims, compiling down to the same [`PredicateExpr`] the matcher already
//! evaluates.
//!
//! ## Grammar (v1, deliberately small — ADR-0031)
//!
//! ```text
//! scope      := comparison ( "AND" comparison )*
//! comparison := IDENT OP ( "claims." IDENT | literal )
//! OP         := "=" | "!=" | "<" | ">" | "<=" | ">="
//! ```
//!
//! `OR`, `NOT`, and parentheses are rejected with [`ScopeError::Unsupported`]
//! — this is a strict AND-only subset, not the full `predicate_compile`
//! grammar. Literal typing matches `predicate_compile.rs`: bare integer →
//! `Number`, decimal → `Float`, `true`/`false` → `Bool`, `'quoted'` or bare
//! identifier → `Text`. A claim always resolves to `ColumnValue::Text` (JWT
//! claims are strings), so a numeric comparison against a claim compares
//! text.
//!
//! ## Fail-closed missing-claim semantics
//!
//! ADR-0031 distinguishes this from ADR-0012's missing-*column* rule: a
//! missing column is a row-data gap (two-valued logic, non-match). A missing
//! claim is a *request-context* gap — the rule cannot be evaluated at all, so
//! [`ScopeExpr::resolve`] denies the entire table (`None`) rather than
//! falling back to `PredicateExpr::any()`. A missing claim must never
//! accidentally widen visibility.

use crate::predicate::{ColumnValue, PredicateExpr};
use crate::principal::Principal;

/// A parse error — the input wasn't a valid v1 scope expression.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScopeError {
    #[error("empty scope expression")]
    Empty,
    #[error("unexpected token `{0}`")]
    UnexpectedToken(String),
    #[error("`{0}` is not supported in a v1 rules scope (AND-composition only)")]
    Unsupported(String),
    #[error("expected a comparison operator after `{0}`")]
    MissingOperator(String),
    #[error("expected a value after `{0}`")]
    MissingValue(String),
}

/// A resolved-at-request-time reference to a principal claim, or a constant.
///
/// `PartialEq` only (no `Eq`): `Literal` wraps [`ColumnValue`], which cannot
/// derive `Eq` because its `Float(f64)` variant has none — the same
/// precedent `PredicateExpr`/`PredicateFilter` already set in
/// `crate::predicate`.
#[derive(Debug, Clone, PartialEq)]
pub enum ScopeValue {
    /// `claims.org_id` — resolved from the connecting principal at request
    /// time. Always resolves to `ColumnValue::Text` (JWT claims are strings),
    /// so e.g. `age > claims.min_age` compares text, not a number.
    Claim(String),
    /// `'open'`, `3`, `true` — a constant, typed like `predicate_compile.rs`.
    Literal(ColumnValue),
}

/// One `column <op> value` comparison.
#[derive(Debug, Clone, PartialEq)]
pub struct ScopeTerm {
    pub column: String,
    pub op: ScopeOp,
    pub value: ScopeValue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeOp {
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
}

impl ScopeOp {
    fn as_str(self) -> &'static str {
        match self {
            ScopeOp::Eq => "=",
            ScopeOp::Ne => "!=",
            ScopeOp::Lt => "<",
            ScopeOp::Gt => ">",
            ScopeOp::Le => "<=",
            ScopeOp::Ge => ">=",
        }
    }
}

/// A parsed scope: one or more terms, AND-composed. Empty vec == unscoped.
///
/// `PartialEq` only, for the same reason as [`ScopeValue`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ScopeExpr {
    pub terms: Vec<ScopeTerm>,
}

impl ScopeExpr {
    /// Parse the v1 grammar (module docs). `OR`, `NOT`, and parentheses are
    /// rejected with `Unsupported`.
    pub fn parse(input: &str) -> Result<Self, ScopeError> {
        let tokens = tokenize(input)?;
        if tokens.is_empty() {
            return Err(ScopeError::Empty);
        }
        let mut p = Parser { tokens, pos: 0 };
        let mut terms = vec![p.parse_comparison()?];
        while matches!(p.peek(), Some(Token::And)) {
            p.advance();
            terms.push(p.parse_comparison()?);
        }
        if let Some(tok) = p.peek() {
            return Err(match tok {
                Token::Or => ScopeError::Unsupported("OR".to_string()),
                Token::Not => ScopeError::Unsupported("NOT".to_string()),
                Token::LParen => ScopeError::Unsupported("(".to_string()),
                Token::RParen => ScopeError::Unsupported(")".to_string()),
                other => ScopeError::UnexpectedToken(format!("{other:?}")),
            });
        }
        Ok(ScopeExpr { terms })
    }

    /// Resolve against a principal. `None` = **deny this table**: a
    /// referenced claim is absent, so no row can be proven in scope.
    /// `Some(any())` is returned only for a genuinely empty scope
    /// (`terms.is_empty()`).
    #[must_use]
    pub fn resolve(&self, principal: &Principal) -> Option<PredicateExpr> {
        if self.terms.is_empty() {
            return Some(PredicateExpr::any());
        }
        // A plain for-loop (not filter_map) so a missing claim on ANY term —
        // not just the first — aborts the whole resolve via `?`. Silently
        // dropping an unresolvable term would widen visibility instead of
        // denying it (the fail-closed property this type exists to enforce).
        let mut resolved = Vec::with_capacity(self.terms.len());
        for term in &self.terms {
            let value = match &term.value {
                ScopeValue::Literal(v) => v.clone(),
                ScopeValue::Claim(field) => ColumnValue::text(principal.claim(field)?),
            };
            resolved.push(match term.op {
                ScopeOp::Eq => PredicateExpr::eq(term.column.clone(), value),
                ScopeOp::Ne => PredicateExpr::ne(term.column.clone(), value),
                ScopeOp::Lt => PredicateExpr::lt(term.column.clone(), value),
                ScopeOp::Gt => PredicateExpr::gt(term.column.clone(), value),
                ScopeOp::Le => PredicateExpr::le(term.column.clone(), value),
                ScopeOp::Ge => PredicateExpr::ge(term.column.clone(), value),
            });
        }
        Some(if resolved.len() == 1 {
            resolved.pop().expect("len == 1")
        } else {
            PredicateExpr::And(resolved)
        })
    }

    /// The claim names this scope references, sorted and deduped (consumed
    /// by `nostos rules check`, Task 17).
    #[must_use]
    pub fn referenced_claims(&self) -> Vec<&str> {
        let mut claims: Vec<&str> = self
            .terms
            .iter()
            .filter_map(|t| match &t.value {
                ScopeValue::Claim(field) => Some(field.as_str()),
                ScopeValue::Literal(_) => None,
            })
            .collect();
        claims.sort_unstable();
        claims.dedup();
        claims
    }

    /// Stable text form used by the checksum. Uppercase `AND`, single space
    /// around operators, terms sorted by `(column, op, value)`.
    #[must_use]
    pub fn canonical(&self) -> String {
        let mut terms: Vec<&ScopeTerm> = self.terms.iter().collect();
        terms.sort_by_key(|t| (t.column.clone(), t.op.as_str(), value_canonical(&t.value)));
        terms
            .iter()
            .map(|t| {
                format!(
                    "{} {} {}",
                    t.column,
                    t.op.as_str(),
                    value_canonical(&t.value)
                )
            })
            .collect::<Vec<_>>()
            .join(" AND ")
    }
}

fn value_canonical(v: &ScopeValue) -> String {
    match v {
        ScopeValue::Claim(field) => format!("claims.{field}"),
        ScopeValue::Literal(ColumnValue::Text(s)) => format!("'{s}'"),
        ScopeValue::Literal(ColumnValue::Number(n)) => n.to_string(),
        ScopeValue::Literal(ColumnValue::Float(f)) => f.to_string(),
        ScopeValue::Literal(ColumnValue::Bool(b)) => b.to_string(),
        ScopeValue::Literal(ColumnValue::Any) => "*".to_string(),
        // Unreachable in practice: scopes never carry placeholders (only
        // `predicate_compile` emits `Param`, in stream templates — P5 sync
        // streams design, Decision 2). Rendered for exhaustiveness only.
        ScopeValue::Literal(ColumnValue::Param(name)) => format!(":{name}"),
    }
}

// --- literal typing (mirrors predicate_compile.rs's classify_literal) -----

/// Both a quoted `'true'` and a bare `true` classify identically — matching
/// `predicate_compile.rs`'s `parse_literal` quirk exactly, so a scope and a
/// predicate string never disagree about what a bare word means.
fn classify_literal(s: String) -> ColumnValue {
    match s.as_str() {
        "true" => ColumnValue::Bool(true),
        "false" => ColumnValue::Bool(false),
        _ => ColumnValue::Text(s),
    }
}

// --- tokenizer --------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Ident(String),
    /// A validated `claims.<field>` reference — the dot-prefix check happens
    /// once, here, so the parser never sees an ill-formed dotted word.
    Claim(String),
    NumberInt(i64),
    NumberFloat(f64),
    Text(String),
    Op(&'static str),
    And,
    Or,
    Not,
    LParen,
    RParen,
}

fn tokenize(input: &str) -> Result<Vec<Token>, ScopeError> {
    let mut tokens = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            ' ' | '\t' | '\n' | '\r' => i += 1,
            '(' => {
                tokens.push(Token::LParen);
                i += 1;
            }
            ')' => {
                tokens.push(Token::RParen);
                i += 1;
            }
            '\'' => {
                i += 1;
                let start = i;
                while i < chars.len() && chars[i] != '\'' {
                    i += 1;
                }
                if i >= chars.len() {
                    return Err(ScopeError::UnexpectedToken(
                        "<unterminated string>".to_string(),
                    ));
                }
                let s: String = chars[start..i].iter().collect();
                tokens.push(Token::Text(s));
                i += 1;
            }
            '=' | '!' | '<' | '>' => {
                let two_char = i + 1 < chars.len() && chars[i + 1] == '=';
                let op: &'static str = match (c, two_char) {
                    ('=', _) => {
                        i += 1;
                        "="
                    }
                    ('!', true) => {
                        i += 2;
                        "!="
                    }
                    ('<', true) => {
                        i += 2;
                        "<="
                    }
                    ('>', true) => {
                        i += 2;
                        ">="
                    }
                    ('<', false) => {
                        i += 1;
                        "<"
                    }
                    ('>', false) => {
                        i += 1;
                        ">"
                    }
                    ('!', false) => {
                        return Err(ScopeError::UnexpectedToken("!".to_string()));
                    }
                    _ => unreachable!(),
                };
                tokens.push(Token::Op(op));
            }
            _ if c.is_ascii_digit()
                || (c == '-' && i + 1 < chars.len() && chars[i + 1].is_ascii_digit()) =>
            {
                let start = i;
                if chars[i] == '-' {
                    i += 1;
                }
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
                let mut is_float = false;
                if i < chars.len() && chars[i] == '.' {
                    is_float = true;
                    i += 1;
                    while i < chars.len() && chars[i].is_ascii_digit() {
                        i += 1;
                    }
                }
                let s: String = chars[start..i].iter().collect();
                if is_float {
                    let n: f64 = s
                        .parse()
                        .map_err(|_| ScopeError::UnexpectedToken(s.clone()))?;
                    tokens.push(Token::NumberFloat(n));
                } else {
                    let n: i64 = s
                        .parse()
                        .map_err(|_| ScopeError::UnexpectedToken(s.clone()))?;
                    tokens.push(Token::NumberInt(n));
                }
            }
            _ if c.is_alphabetic() || c == '_' => {
                let start = i;
                while i < chars.len()
                    && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '.')
                {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                let upper = word.to_ascii_uppercase();
                let tok = match upper.as_str() {
                    "AND" => Token::And,
                    "OR" => Token::Or,
                    "NOT" => Token::Not,
                    // Case-insensitive, mirroring predicate_compile.rs's own
                    // tokenizer: a *bare* TRUE/FALSE (any case) normalizes to
                    // Bool. A *quoted* 'TRUE' does not — quoting is an
                    // explicit text intent, so only the exact lowercase
                    // spelling matches in classify_literal. Without this
                    // branch, `flag != TRUE` tokenized as Ident("TRUE") and
                    // classify_literal fell through to Text("TRUE"); against
                    // a real Bool column, matches_value has no (Bool, Text)
                    // arm and defaults to non-match, so `!=` wrongly widened
                    // to true — a widening bug in an access-control grammar.
                    "TRUE" => Token::Text("true".to_string()),
                    "FALSE" => Token::Text("false".to_string()),
                    _ if word.contains('.') => match word.strip_prefix("claims.") {
                        Some(field) if !field.is_empty() && !field.contains('.') => {
                            Token::Claim(field.to_string())
                        }
                        _ => return Err(ScopeError::UnexpectedToken(word)),
                    },
                    _ => Token::Ident(word),
                };
                tokens.push(tok);
            }
            _ => {
                return Err(ScopeError::UnexpectedToken(c.to_string()));
            }
        }
    }
    Ok(tokens)
}

// --- linear parser (AND-only: no recursion needed) -------------------

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn advance(&mut self) -> Option<Token> {
        let t = self.tokens.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    /// comparison := IDENT OP ( "claims." IDENT | literal )
    fn parse_comparison(&mut self) -> Result<ScopeTerm, ScopeError> {
        let column = match self.advance() {
            Some(Token::Ident(s)) => s,
            Some(Token::Not) => return Err(ScopeError::Unsupported("NOT".to_string())),
            Some(Token::Or) => return Err(ScopeError::Unsupported("OR".to_string())),
            Some(Token::LParen) => return Err(ScopeError::Unsupported("(".to_string())),
            Some(Token::RParen) => return Err(ScopeError::Unsupported(")".to_string())),
            other => return Err(ScopeError::UnexpectedToken(format!("{other:?}"))),
        };
        let op = match self.advance() {
            Some(Token::Op("=")) => ScopeOp::Eq,
            Some(Token::Op("!=")) => ScopeOp::Ne,
            Some(Token::Op("<")) => ScopeOp::Lt,
            Some(Token::Op(">")) => ScopeOp::Gt,
            Some(Token::Op("<=")) => ScopeOp::Le,
            Some(Token::Op(">=")) => ScopeOp::Ge,
            Some(Token::Op(_)) => unreachable!("tokenize only emits the six known ops"),
            None => return Err(ScopeError::MissingOperator(column)),
            Some(other) => return Err(ScopeError::MissingOperator(format!("{other:?}"))),
        };
        let value = match self.advance() {
            Some(Token::Claim(field)) => ScopeValue::Claim(field),
            Some(Token::NumberInt(n)) => ScopeValue::Literal(ColumnValue::Number(n)),
            Some(Token::NumberFloat(n)) => ScopeValue::Literal(ColumnValue::Float(n)),
            Some(Token::Text(s) | Token::Ident(s)) => ScopeValue::Literal(classify_literal(s)),
            None => return Err(ScopeError::MissingValue(op.as_str().to_string())),
            Some(other) => return Err(ScopeError::MissingValue(format!("{other:?}"))),
        };
        Ok(ScopeTerm { column, op, value })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn principal_with(claims: &[(&str, &str)]) -> Principal {
        let extra: BTreeMap<String, String> = claims
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        Principal::with_claims("u1", "t1", extra)
    }

    #[test]
    fn parses_single_claim_term() {
        let scope = ScopeExpr::parse("owner_id = claims.sub").unwrap();
        assert_eq!(
            scope.terms,
            vec![ScopeTerm {
                column: "owner_id".to_string(),
                op: ScopeOp::Eq,
                value: ScopeValue::Claim("sub".to_string()),
            }]
        );
    }

    #[test]
    fn parses_and_composition() {
        let scope = ScopeExpr::parse("org_id = claims.org_id AND status != 'archived'").unwrap();
        assert_eq!(scope.terms.len(), 2);
        assert_eq!(
            scope.terms[1].value,
            ScopeValue::Literal(ColumnValue::Text("archived".to_string()))
        );
    }

    #[test]
    fn rejects_or() {
        assert_eq!(
            ScopeExpr::parse("a = 1 OR b = 2"),
            Err(ScopeError::Unsupported("OR".to_string()))
        );
    }

    #[test]
    fn rejects_not() {
        assert_eq!(
            ScopeExpr::parse("NOT a = 1"),
            Err(ScopeError::Unsupported("NOT".to_string()))
        );
    }

    #[test]
    fn rejects_parens() {
        assert_eq!(
            ScopeExpr::parse("(a = 1)"),
            Err(ScopeError::Unsupported("(".to_string()))
        );
    }

    #[test]
    fn rejects_lone_operator() {
        assert!(matches!(
            ScopeExpr::parse("owner_id ="),
            Err(ScopeError::MissingValue(_) | ScopeError::MissingOperator(_))
        ));
    }

    #[test]
    fn rejects_non_claims_dotted_ident() {
        // A typo ("claim." not "claims.") must not silently become a Text
        // literal — that would make `owner_id = claim.sub` a rule that never
        // matches instead of a config error the operator can see.
        assert!(matches!(
            ScopeExpr::parse("owner_id = claim.sub"),
            Err(ScopeError::UnexpectedToken(_))
        ));
    }

    #[test]
    fn resolve_missing_claim_denies() {
        let scope = ScopeExpr::parse("org_id = claims.org_id").unwrap();
        let principal = principal_with(&[]);
        assert_eq!(scope.resolve(&principal), None);
    }

    #[test]
    fn resolve_denies_when_second_term_claim_missing() {
        // The fail-closed guarantee must hold for every term, not just the
        // first — a filter_map/flat_map implementation would silently drop
        // an unresolvable second term and widen visibility instead.
        let scope = ScopeExpr::parse("status = 'open' AND org_id = claims.org_id").unwrap();
        let principal = principal_with(&[]);
        assert_eq!(scope.resolve(&principal), None);
    }

    #[test]
    fn resolve_builds_and_chain() {
        let scope = ScopeExpr::parse("a = 1 AND b = 2").unwrap();
        let principal = principal_with(&[]);
        assert_eq!(
            scope.resolve(&principal),
            Some(PredicateExpr::And(vec![
                PredicateExpr::eq("a", ColumnValue::number(1)),
                PredicateExpr::eq("b", ColumnValue::number(2)),
            ]))
        );
    }

    #[test]
    fn parses_case_insensitive_bool_literal() {
        // Mirrors predicate_compile.rs: a bare TRUE/FALSE of any case types
        // to Bool, matching the module doc's claim that literal typing
        // matches predicate_compile.rs exactly.
        let scope = ScopeExpr::parse("flag = TRUE").unwrap();
        assert_eq!(
            scope.terms[0].value,
            ScopeValue::Literal(ColumnValue::boolean(true))
        );
        let scope = ScopeExpr::parse("flag = False").unwrap();
        assert_eq!(
            scope.terms[0].value,
            ScopeValue::Literal(ColumnValue::boolean(false))
        );
    }

    #[test]
    fn quoted_uppercase_true_stays_text() {
        // Quoting is an explicit text intent (same as predicate_compile.rs):
        // only a bare, unquoted TRUE/FALSE normalizes to Bool.
        let scope = ScopeExpr::parse("flag = 'TRUE'").unwrap();
        assert_eq!(
            scope.terms[0].value,
            ScopeValue::Literal(ColumnValue::text("TRUE"))
        );
    }

    #[test]
    fn uppercase_bool_literal_does_not_widen_ne_comparison() {
        // Regression (Task 3 review, Important finding): before the
        // tokenizer's case-insensitive TRUE/FALSE branch, `flag != TRUE`
        // mistyped TRUE as Text("TRUE"). PredicateExpr's (Bool, Text)
        // mismatch defaults to non-equal, so `!=` wrongly matched a row
        // where flag is really true — a widening bug in an access-control
        // grammar. This must stay `false` (no match) for a row that IS true.
        let scope = ScopeExpr::parse("flag != TRUE").unwrap();
        let principal = principal_with(&[]);
        let expr = scope.resolve(&principal).expect("no claims referenced");
        let row_flag_true = |col: &str| (col == "flag").then_some(ColumnValue::boolean(true));
        assert!(
            !expr.matches(row_flag_true),
            "flag != TRUE must not match a row where flag is true"
        );
    }

    #[test]
    fn canonical_is_order_insensitive() {
        let a = ScopeExpr::parse("a = 1 AND b = 2").unwrap();
        let b = ScopeExpr::parse("b=2   AND   a=1").unwrap();
        assert_eq!(a.canonical(), b.canonical());
        assert_eq!(a.canonical(), "a = 1 AND b = 2");
    }
}
