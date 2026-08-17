//! Safe-SQL-subset predicate compiler (ADR-0012 — the client-facing entry point).
//!
//! Parses a small, safe subset of SQL-like predicate syntax into the existing
//! [`PredicateExpr`] tree — the "dynamic" in "dynamic reactive sync." A client
//! sends `status = open AND priority >= 3` and the server compiles it into the
//! tree the matcher evaluates. No SQL execution, no injection surface: the
//! output is a pure data structure, never a query string.
//!
//! ## Grammar (recursive descent)
//!
//! ```text
//! expr       := or_expr
//! or_expr    := and_expr ( "OR" and_expr )*
//! and_expr   := not_expr ( "AND" not_expr )*
//! not_expr   := "NOT" not_expr | atom
//! atom       := "(" expr ")" | comparison
//! comparison := IDENT OP literal
//! literal    := NUMBER | FLOAT | "true" | "false" | "'TEXT'" | IDENT | PARAM
//! PARAM      := ":" IDENT   (placeholder — literal position ONLY)
//! OP         := "=" | "!=" | "<" | ">" | "<=" | ">="
//! ```
//!
//! Precedence (standard): `NOT` > `AND` > `OR`; parentheses override.
//!
//! ## Placeholders (P5 sync streams)
//!
//! A server-side stream template may use `:name` placeholders in LITERAL
//! position only — the right-hand side of a comparison (design:
//! docs/plans/p5-sync-streams-design.md, Decisions 1-2). A placeholder parses
//! to a [`ColumnValue::Param`] marker leaf; [`bind_params`] substitutes typed
//! values at subscribe time (value-level binding — no client byte ever enters
//! a query string, the injection answer). A placeholder in column position or
//! standing alone is a parse error; inside a quoted string it is ordinary
//! text, never a placeholder.
//!
//! ## Literal typing
//!
//! Auto-inferred like the extractor's coercion (ADR-0012 slice 2): bare integer
//! `3` → `Number`, decimal `3.5` → `Float`, `true`/`false` → `Bool`, single-
//! quoted `'open'` or bare identifier `open` → `Text` (bare-ident-as-text
//! matches the JSON payload reality where every value is a string).
//!
//! ## Scope (deliberately bounded)
//!
//! Six comparison operators + `AND`/`OR`/`NOT` + parens. No `IN`/`LIKE`/
//! `BETWEEN`/`IS NULL`/joins/aggregates — add when a real query demands. The AST
//! is the existing `PredicateExpr` (no new types).

use std::collections::{HashMap, HashSet};

use crate::predicate::{ColumnValue, PredicateExpr, PredicateFilter};

/// A parse error — the input wasn't a valid predicate in the safe subset.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("empty predicate")]
    Empty,
    #[error("unexpected end of input; expected {expected}")]
    UnexpectedEof { expected: &'static str },
    #[error("unexpected token {found:?}; expected {expected}")]
    UnexpectedToken {
        found: String,
        expected: &'static str,
    },
    #[error("unbalanced parentheses")]
    UnbalancedParens,
    #[error("trailing input after predicate: {0}")]
    TrailingInput(String),
    #[error("invalid number literal: {0}")]
    InvalidNumber(String),
}

/// Parse a safe-SQL-subset predicate string into a [`PredicateExpr`] tree.
///
/// Returns the root expression, or a [`ParseError`] describing what was wrong.
/// The output is a pure data structure — never a SQL query — so there is no
/// injection surface.
pub fn parse_predicate_expr(input: &str) -> Result<PredicateExpr, ParseError> {
    let tokens = tokenize(input)?;
    if tokens.is_empty() {
        return Err(ParseError::Empty);
    }
    let mut p = Parser { tokens, pos: 0 };
    let expr = p.parse_or()?;
    // All input must be consumed (no trailing garbage).
    if p.pos < p.tokens.len() {
        return Err(ParseError::TrailingInput(format!("{:?}", p.tokens[p.pos])));
    }
    Ok(expr)
}

// --- tokenizer ------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Ident(String),
    NumberInt(i64),
    NumberFloat(f64),
    Text(String),     // single-quoted string
    Param(String),    // `:ident` placeholder (P5 sync streams; literal position only)
    Op(&'static str), // one of the 6 comparison operators
    And,
    Or,
    Not,
    LParen,
    RParen,
}

fn tokenize(input: &str) -> Result<Vec<Token>, ParseError> {
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
                // Single-quoted string literal.
                i += 1;
                let start = i;
                while i < chars.len() && chars[i] != '\'' {
                    i += 1;
                }
                if i >= chars.len() {
                    return Err(ParseError::UnexpectedEof {
                        expected: "closing '",
                    });
                }
                let s: String = chars[start..i].iter().collect();
                tokens.push(Token::Text(s));
                i += 1; // consume closing quote
            }
            ':' => {
                // `:ident` placeholder (P5 sync streams). The token is
                // position-agnostic; the PARSER restricts it to literal
                // position by only accepting it in `parse_literal`.
                let start = i + 1;
                let mut j = start;
                while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_') {
                    j += 1;
                }
                if j == start || !(chars[start].is_alphabetic() || chars[start] == '_') {
                    return Err(ParseError::UnexpectedToken {
                        found: ":".into(),
                        expected: "an identifier after ':'",
                    });
                }
                let name: String = chars[start..j].iter().collect();
                tokens.push(Token::Param(name));
                i = j;
            }
            '=' | '!' | '<' | '>' => {
                // Two-char operators: !=, <=, >=. Single-char: =, <, >.
                let op = if i + 1 < chars.len() && chars[i + 1] == '=' {
                    let two = match c {
                        '!' => "!=",
                        '<' => "<=",
                        '>' => ">=",
                        '=' => "=", // = = is not valid; handle below
                        _ => unreachable!(),
                    };
                    if c == '=' {
                        // '= =' — treat single '=' only
                        tokens.push(Token::Op("="));
                        i += 1;
                        continue;
                    }
                    i += 2;
                    two
                } else {
                    let one = match c {
                        '=' => "=",
                        '<' => "<",
                        '>' => ">",
                        '!' => {
                            return Err(ParseError::UnexpectedToken {
                                found: "!".into(),
                                expected: "!=",
                            })
                        }
                        _ => unreachable!(),
                    };
                    i += 1;
                    one
                };
                tokens.push(Token::Op(op));
            }
            _ if c.is_ascii_digit()
                || (c == '-' && i + 1 < chars.len() && chars[i + 1].is_ascii_digit()) =>
            {
                // Number literal (integer or float).
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
                        .map_err(|_| ParseError::InvalidNumber(s.clone()))?;
                    tokens.push(Token::NumberFloat(n));
                } else {
                    let n: i64 = s
                        .parse()
                        .map_err(|_| ParseError::InvalidNumber(s.clone()))?;
                    tokens.push(Token::NumberInt(n));
                }
            }
            _ if c.is_alphabetic() || c == '_' => {
                // Identifier or keyword (AND/OR/NOT/true/false).
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                let upper = word.to_ascii_uppercase();
                let tok = match upper.as_str() {
                    "AND" => Token::And,
                    "OR" => Token::Or,
                    "NOT" => Token::Not,
                    // Encode booleans as Text("true"/"false") — parse_literal
                    // promotes them to ColumnValue::Bool. Keeps the token enum
                    // minimal while preserving correct typed matching.
                    "TRUE" => Token::Text("true".into()),
                    "FALSE" => Token::Text("false".into()),
                    _ => Token::Ident(word),
                };
                tokens.push(tok);
            }
            _ => {
                return Err(ParseError::UnexpectedToken {
                    found: c.to_string(),
                    expected: "a predicate token",
                });
            }
        }
    }
    Ok(tokens)
}

// --- recursive-descent parser ---------------------------------------------

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

    /// or_expr := and_expr ( "OR" and_expr )*
    fn parse_or(&mut self) -> Result<PredicateExpr, ParseError> {
        let first = self.parse_and()?;
        let mut parts = vec![first];
        while matches!(self.peek(), Some(Token::Or)) {
            self.advance();
            parts.push(self.parse_and()?);
        }
        Ok(if parts.len() == 1 {
            parts.pop().unwrap()
        } else {
            PredicateExpr::Or(parts)
        })
    }

    /// and_expr := not_expr ( "AND" not_expr )*
    fn parse_and(&mut self) -> Result<PredicateExpr, ParseError> {
        let first = self.parse_not()?;
        let mut parts = vec![first];
        while matches!(self.peek(), Some(Token::And)) {
            self.advance();
            parts.push(self.parse_not()?);
        }
        Ok(if parts.len() == 1 {
            parts.pop().unwrap()
        } else {
            PredicateExpr::And(parts)
        })
    }

    /// not_expr := "NOT" not_expr | atom
    fn parse_not(&mut self) -> Result<PredicateExpr, ParseError> {
        if matches!(self.peek(), Some(Token::Not)) {
            self.advance();
            let inner = self.parse_not()?;
            return Ok(PredicateExpr::Not(Box::new(inner)));
        }
        self.parse_atom()
    }

    /// atom := "(" expr ")" | comparison
    fn parse_atom(&mut self) -> Result<PredicateExpr, ParseError> {
        match self.peek() {
            Some(Token::LParen) => {
                self.advance();
                let inner = self.parse_or()?;
                match self.advance() {
                    Some(Token::RParen) => Ok(inner),
                    _ => Err(ParseError::UnbalancedParens),
                }
            }
            Some(Token::Ident(_)) => self.parse_comparison(),
            other => Err(ParseError::UnexpectedToken {
                found: format!("{other:?}"),
                expected: "an identifier or '('",
            }),
        }
    }

    /// comparison := IDENT OP literal
    fn parse_comparison(&mut self) -> Result<PredicateExpr, ParseError> {
        let column = match self.advance() {
            Some(Token::Ident(s)) => s,
            other => {
                return Err(ParseError::UnexpectedToken {
                    found: format!("{other:?}"),
                    expected: "a column identifier",
                })
            }
        };
        let op = match self.advance() {
            Some(Token::Op(o)) => o,
            other => {
                return Err(ParseError::UnexpectedToken {
                    found: format!("{other:?}"),
                    expected: "a comparison operator (=, !=, <, >, <=, >=)",
                })
            }
        };
        let value = self.parse_literal()?;
        let filter = PredicateFilter { column, value };
        let leaf = match op {
            "=" => PredicateExpr::Eq(filter),
            "!=" => PredicateExpr::Ne(filter),
            "<" => PredicateExpr::Lt(filter),
            ">" => PredicateExpr::Gt(filter),
            "<=" => PredicateExpr::Le(filter),
            ">=" => PredicateExpr::Ge(filter),
            _ => unreachable!("tokenize only emits valid ops"),
        };
        Ok(leaf)
    }

    /// literal := NUMBER | FLOAT | "'TEXT'" | IDENT | PARAM
    /// (true/false were tokenized as Text("true"/"false") — but typed comparison
    /// needs a Bool. Detect them here and emit Bool; everything else is Text.)
    ///
    /// This is the ONLY position a `Param` token is accepted — a placeholder
    /// in column position or standing alone fails parsing here or in
    /// `parse_atom`/`parse_comparison`, so an illegally-placed `Param` can
    /// never survive into a parsed tree (P5 sync streams, Decision 2).
    fn parse_literal(&mut self) -> Result<ColumnValue, ParseError> {
        match self.advance() {
            Some(Token::Param(name)) => Ok(ColumnValue::Param(name)),
            Some(Token::NumberInt(n)) => Ok(ColumnValue::Number(n)),
            Some(Token::NumberFloat(n)) => Ok(ColumnValue::Float(n)),
            Some(Token::Text(s)) => {
                // true/false literals → Bool (typed comparison). Other text → Text.
                match s.as_str() {
                    "true" => Ok(ColumnValue::Bool(true)),
                    "false" => Ok(ColumnValue::Bool(false)),
                    _ => Ok(ColumnValue::Text(s)),
                }
            }
            Some(Token::Ident(s)) => {
                // Bare identifier as a value is text (e.g. status = open).
                match s.as_str() {
                    "true" => Ok(ColumnValue::Bool(true)),
                    "false" => Ok(ColumnValue::Bool(false)),
                    _ => Ok(ColumnValue::Text(s)),
                }
            }
            other => Err(ParseError::UnexpectedToken {
                found: format!("{other:?}"),
                expected: "a literal (number, quoted text, true/false, or identifier)",
            }),
        }
    }
}

// --- parameter binding (P5 sync streams) ------------------------------------

/// A bind error — the params object didn't match the template's placeholders.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BindError {
    #[error("missing value for placeholder :{name}")]
    MissingParam { name: String },
    #[error("unexpected param {name:?}: no placeholder :{name} in the template")]
    ExtraParam { name: String },
    #[error("param {name:?} is itself a placeholder; params must bind to concrete values")]
    NestedParam { name: String },
}

/// Substitute every [`ColumnValue::Param`] placeholder leaf in `expr` with its
/// typed value from `params` (P5 sync streams, docs/plans/p5-sync-streams-
/// design.md Decision 2: binding is value-level, never textual).
///
/// The template is parsed ONCE at server startup; this runs per subscribe with
/// the client's params. Binding is strict and loud, mirroring the
/// `InvalidWhereSql` reject at the transport boundary:
///
/// - every placeholder must have a param ([`BindError::MissingParam`]);
/// - every param must be used by the template ([`BindError::ExtraParam`]);
/// - a param value that is itself a `Param` is rejected
///   ([`BindError::NestedParam`]), so a successfully bound tree contains NO
///   placeholder leaves.
///
/// Param names are ordinary strings at this layer — a param naming the tenant
/// column binds like any other; the tenant override lives in transport
/// (design Decision 3), not here.
pub fn bind_params<S: std::hash::BuildHasher>(
    expr: &PredicateExpr,
    params: &HashMap<String, ColumnValue, S>,
) -> Result<PredicateExpr, BindError> {
    let mut used = HashSet::new();
    let bound = bind_expr(expr, params, &mut used)?;
    for name in params.keys() {
        if !used.contains(name) {
            return Err(BindError::ExtraParam { name: name.clone() });
        }
    }
    Ok(bound)
}

fn bind_expr<S: std::hash::BuildHasher>(
    expr: &PredicateExpr,
    params: &HashMap<String, ColumnValue, S>,
    used: &mut HashSet<String>,
) -> Result<PredicateExpr, BindError> {
    match expr {
        PredicateExpr::Any => Ok(PredicateExpr::Any),
        PredicateExpr::Eq(f) => Ok(PredicateExpr::Eq(bind_filter(f, params, used)?)),
        PredicateExpr::Ne(f) => Ok(PredicateExpr::Ne(bind_filter(f, params, used)?)),
        PredicateExpr::Lt(f) => Ok(PredicateExpr::Lt(bind_filter(f, params, used)?)),
        PredicateExpr::Gt(f) => Ok(PredicateExpr::Gt(bind_filter(f, params, used)?)),
        PredicateExpr::Le(f) => Ok(PredicateExpr::Le(bind_filter(f, params, used)?)),
        PredicateExpr::Ge(f) => Ok(PredicateExpr::Ge(bind_filter(f, params, used)?)),
        PredicateExpr::And(parts) => parts
            .iter()
            .map(|p| bind_expr(p, params, used))
            .collect::<Result<Vec<_>, _>>()
            .map(PredicateExpr::And),
        PredicateExpr::Or(parts) => parts
            .iter()
            .map(|p| bind_expr(p, params, used))
            .collect::<Result<Vec<_>, _>>()
            .map(PredicateExpr::Or),
        PredicateExpr::Not(inner) => Ok(PredicateExpr::Not(Box::new(bind_expr(
            inner, params, used,
        )?))),
    }
}

fn bind_filter<S: std::hash::BuildHasher>(
    f: &PredicateFilter,
    params: &HashMap<String, ColumnValue, S>,
    used: &mut HashSet<String>,
) -> Result<PredicateFilter, BindError> {
    let value = match &f.value {
        ColumnValue::Param(name) => {
            let v = params
                .get(name)
                .ok_or_else(|| BindError::MissingParam { name: name.clone() })?;
            if matches!(v, ColumnValue::Param(_)) {
                return Err(BindError::NestedParam { name: name.clone() });
            }
            used.insert(name.clone());
            v.clone()
        }
        other => other.clone(),
    };
    Ok(PredicateFilter {
        column: f.column.clone(),
        value,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::predicate::ColumnValue;

    fn row(pairs: &[(&str, ColumnValue)]) -> impl Fn(&str) -> Option<ColumnValue> {
        let owned: Vec<(String, ColumnValue)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect();
        move |col: &str| owned.iter().find(|(k, _)| k == col).map(|(_, v)| v.clone())
    }

    // ---- operators ----

    #[test]
    fn equality_operator() {
        let e = parse_predicate_expr("status = open").unwrap();
        assert_eq!(e, PredicateExpr::eq("status", ColumnValue::text("open")));
    }

    #[test]
    fn all_six_operators() {
        assert_eq!(
            parse_predicate_expr("a = 1").unwrap(),
            PredicateExpr::eq("a", ColumnValue::number(1))
        );
        assert_eq!(
            parse_predicate_expr("a != 1").unwrap(),
            PredicateExpr::ne("a", ColumnValue::number(1))
        );
        assert_eq!(
            parse_predicate_expr("a < 1").unwrap(),
            PredicateExpr::lt("a", ColumnValue::number(1))
        );
        assert_eq!(
            parse_predicate_expr("a > 1").unwrap(),
            PredicateExpr::gt("a", ColumnValue::number(1))
        );
        assert_eq!(
            parse_predicate_expr("a <= 1").unwrap(),
            PredicateExpr::le("a", ColumnValue::number(1))
        );
        assert_eq!(
            parse_predicate_expr("a >= 1").unwrap(),
            PredicateExpr::ge("a", ColumnValue::number(1))
        );
    }

    // ---- literal types ----

    #[test]
    fn literal_types() {
        assert_eq!(
            parse_predicate_expr("a = 42").unwrap(),
            PredicateExpr::eq("a", ColumnValue::number(42))
        );
        assert_eq!(
            parse_predicate_expr("a = 3.5").unwrap(),
            PredicateExpr::eq("a", ColumnValue::float(3.5))
        );
        assert_eq!(
            parse_predicate_expr("a = true").unwrap(),
            PredicateExpr::eq("a", ColumnValue::boolean(true))
        );
        assert_eq!(
            parse_predicate_expr("a = 'open'").unwrap(),
            PredicateExpr::eq("a", ColumnValue::text("open"))
        );
        // Negative number.
        assert_eq!(
            parse_predicate_expr("a > -5").unwrap(),
            PredicateExpr::gt("a", ColumnValue::number(-5))
        );
    }

    // ---- boolean combinators ----

    #[test]
    fn and_conjunction() {
        let e = parse_predicate_expr("a = 1 AND b = 2").unwrap();
        assert_eq!(
            e,
            PredicateExpr::And(vec![
                PredicateExpr::eq("a", ColumnValue::number(1)),
                PredicateExpr::eq("b", ColumnValue::number(2)),
            ])
        );
    }

    #[test]
    fn or_disjunction() {
        let e = parse_predicate_expr("a = 1 OR a = 2").unwrap();
        assert_eq!(
            e,
            PredicateExpr::Or(vec![
                PredicateExpr::eq("a", ColumnValue::number(1)),
                PredicateExpr::eq("a", ColumnValue::number(2)),
            ])
        );
    }

    #[test]
    fn not_negation() {
        let e = parse_predicate_expr("NOT a = 1").unwrap();
        assert_eq!(
            e,
            PredicateExpr::Not(Box::new(PredicateExpr::eq("a", ColumnValue::number(1))))
        );
    }

    // ---- precedence + parens ----

    #[test]
    fn and_binds_tighter_than_or() {
        // a=1 AND b=2 OR c=3  =>  Or([ And([a=1, b=2]), c=3 ])
        let e = parse_predicate_expr("a = 1 AND b = 2 OR c = 3").unwrap();
        assert_eq!(
            e,
            PredicateExpr::Or(vec![
                PredicateExpr::And(vec![
                    PredicateExpr::eq("a", ColumnValue::number(1)),
                    PredicateExpr::eq("b", ColumnValue::number(2)),
                ]),
                PredicateExpr::eq("c", ColumnValue::number(3)),
            ])
        );
    }

    #[test]
    fn parens_override_precedence() {
        // (a=1 OR b=2) AND c=3  =>  And([ Or([a=1, b=2]), c=3 ])
        let e = parse_predicate_expr("(a = 1 OR b = 2) AND c = 3").unwrap();
        assert_eq!(
            e,
            PredicateExpr::And(vec![
                PredicateExpr::Or(vec![
                    PredicateExpr::eq("a", ColumnValue::number(1)),
                    PredicateExpr::eq("b", ColumnValue::number(2)),
                ]),
                PredicateExpr::eq("c", ColumnValue::number(3)),
            ])
        );
    }

    #[test]
    fn not_binds_tighter_than_and() {
        // NOT a = 1 AND b = 2  =>  And([ Not(a=1), b=2 ])
        let e = parse_predicate_expr("NOT a = 1 AND b = 2").unwrap();
        assert_eq!(
            e,
            PredicateExpr::And(vec![
                PredicateExpr::Not(Box::new(PredicateExpr::eq("a", ColumnValue::number(1)))),
                PredicateExpr::eq("b", ColumnValue::number(2)),
            ])
        );
    }

    // ---- semantic equivalence (the correctness gate) ----

    #[test]
    fn parsed_predicate_matches_same_rows_as_handbuilt() {
        // The real-world shape: status=open AND priority>=3.
        let parsed = parse_predicate_expr("status = open AND priority >= 3").unwrap();
        let handbuilt = PredicateExpr::And(vec![
            PredicateExpr::eq("status", ColumnValue::text("open")),
            PredicateExpr::ge("priority", ColumnValue::number(3)),
        ]);
        // The two trees must be structurally equal...
        assert_eq!(parsed, handbuilt);
        // ...AND match the same rows (the JSON payload carries priority as text
        // "5", which the typed Ge leaf coerces).
        let matching = row(&[
            ("status", ColumnValue::text("open")),
            ("priority", ColumnValue::text("5")),
        ]);
        let nonmatching = row(&[
            ("status", ColumnValue::text("open")),
            ("priority", ColumnValue::text("2")),
        ]);
        assert!(parsed.matches(&matching));
        assert!(!parsed.matches(&nonmatching));
        assert_eq!(parsed.matches(&matching), handbuilt.matches(&matching));
        assert_eq!(
            parsed.matches(&nonmatching),
            handbuilt.matches(&nonmatching)
        );
    }

    // ---- error cases ----

    #[test]
    fn empty_string_errors() {
        assert_eq!(parse_predicate_expr(""), Err(ParseError::Empty));
        assert_eq!(parse_predicate_expr("   "), Err(ParseError::Empty));
    }

    #[test]
    fn unbalanced_parens_error() {
        assert_eq!(
            parse_predicate_expr("(a = 1"),
            Err(ParseError::UnbalancedParens)
        );
    }

    #[test]
    fn missing_operator_error() {
        assert!(matches!(
            parse_predicate_expr("a 1"),
            Err(ParseError::UnexpectedToken { .. })
        ));
    }

    #[test]
    fn trailing_input_error() {
        assert!(matches!(
            parse_predicate_expr("a = 1 garbage"),
            Err(ParseError::TrailingInput(_))
        ));
    }

    #[test]
    fn lone_operator_error() {
        // "!" without "=" is not a valid operator.
        assert!(matches!(
            parse_predicate_expr("a ! 1"),
            Err(ParseError::UnexpectedToken { .. })
        ));
    }

    // ---- P5 sync streams: `:param` placeholders (design §6) --------------
    // docs/plans/p5-sync-streams-design.md Decisions 1-2: placeholders parse
    // in LITERAL position only, bind value-level at subscribe time, and every
    // mismatch is a loud error — never a silent pass.

    #[test]
    fn param_parses_in_literal_position() {
        let e = parse_predicate_expr("owner = :owner").unwrap();
        assert_eq!(e, PredicateExpr::eq("owner", ColumnValue::param("owner")));
    }

    #[test]
    fn param_in_all_six_operators() {
        assert_eq!(
            parse_predicate_expr("a = :p").unwrap(),
            PredicateExpr::eq("a", ColumnValue::param("p"))
        );
        assert_eq!(
            parse_predicate_expr("a != :p").unwrap(),
            PredicateExpr::ne("a", ColumnValue::param("p"))
        );
        assert_eq!(
            parse_predicate_expr("a < :p").unwrap(),
            PredicateExpr::lt("a", ColumnValue::param("p"))
        );
        assert_eq!(
            parse_predicate_expr("a > :p").unwrap(),
            PredicateExpr::gt("a", ColumnValue::param("p"))
        );
        assert_eq!(
            parse_predicate_expr("a <= :p").unwrap(),
            PredicateExpr::le("a", ColumnValue::param("p"))
        );
        assert_eq!(
            parse_predicate_expr("a >= :p").unwrap(),
            PredicateExpr::ge("a", ColumnValue::param("p"))
        );
    }

    #[test]
    fn param_in_compound_template() {
        // The design's §2 example shape.
        let e = parse_predicate_expr("owner_id = :owner AND priority >= :min").unwrap();
        assert_eq!(
            e,
            PredicateExpr::And(vec![
                PredicateExpr::eq("owner_id", ColumnValue::param("owner")),
                PredicateExpr::ge("priority", ColumnValue::param("min")),
            ])
        );
    }

    #[test]
    fn param_underscore_and_digit_suffix_names() {
        let e = parse_predicate_expr("a = :min_2").unwrap();
        assert_eq!(e, PredicateExpr::eq("a", ColumnValue::param("min_2")));
    }

    #[test]
    fn param_in_column_position_is_a_parse_error() {
        assert!(parse_predicate_expr(":owner = 1").is_err());
    }

    #[test]
    fn param_standing_alone_is_a_parse_error() {
        assert!(parse_predicate_expr(":owner").is_err());
        assert!(parse_predicate_expr("a = 1 AND :owner").is_err());
    }

    #[test]
    fn param_colon_without_identifier_is_a_parse_error() {
        assert!(matches!(
            parse_predicate_expr("a = :"),
            Err(ParseError::UnexpectedToken { .. })
        ));
        // A digit-led name is not an identifier.
        assert!(matches!(
            parse_predicate_expr("a = :1abc"),
            Err(ParseError::UnexpectedToken { .. })
        ));
    }

    #[test]
    fn quoted_colon_ident_is_ordinary_text_not_a_placeholder() {
        let e = parse_predicate_expr("a = ':owner'").unwrap();
        assert_eq!(e, PredicateExpr::eq("a", ColumnValue::text(":owner")));
    }

    #[test]
    fn join_subquery_and_metachar_shapes_stay_rejected() {
        // Startup validation (design §2/§6): the grammar never grew to fit
        // these, so JOIN/CTE/subquery/injection-shaped templates fail loudly
        // at parse time — config errors at boot, never at subscribe.
        assert!(parse_predicate_expr("a = 1 AND b IN (SELECT id FROM t)").is_err());
        assert!(parse_predicate_expr("a = 1 JOIN t ON t.a = 1").is_err());
        assert!(parse_predicate_expr("owner = :owner; DROP TABLE tasks;--").is_err());
        assert!(parse_predicate_expr("owner = :owner UNION SELECT * FROM tasks").is_err());
    }

    // ---- bind_params (design Decision 2) ----

    fn stream_template() -> PredicateExpr {
        parse_predicate_expr("owner = :owner AND priority >= :min").unwrap()
    }

    fn params(pairs: &[(&str, ColumnValue)]) -> HashMap<String, ColumnValue> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn bind_substitutes_typed_leaves_shape_preserved() {
        let bound = bind_params(
            &stream_template(),
            &params(&[
                ("owner", ColumnValue::text("u1")),
                ("min", ColumnValue::number(3)),
            ]),
        )
        .unwrap();
        assert_eq!(
            bound,
            PredicateExpr::And(vec![
                PredicateExpr::eq("owner", ColumnValue::text("u1")),
                PredicateExpr::ge("priority", ColumnValue::number(3)),
            ])
        );
    }

    #[test]
    fn bind_missing_param_is_a_loud_error() {
        let err = bind_params(
            &stream_template(),
            &params(&[("owner", ColumnValue::text("u1"))]),
        )
        .unwrap_err();
        assert_eq!(err, BindError::MissingParam { name: "min".into() });
    }

    #[test]
    fn bind_extra_param_is_a_loud_error() {
        // The abuse shape: a param the template never asked for (e.g. aimed at
        // the tenant column) never silently passes through.
        let err = bind_params(
            &stream_template(),
            &params(&[
                ("owner", ColumnValue::text("u1")),
                ("min", ColumnValue::number(3)),
                ("org_id", ColumnValue::text("tenant-b")),
            ]),
        )
        .unwrap_err();
        assert_eq!(
            err,
            BindError::ExtraParam {
                name: "org_id".into()
            }
        );
    }

    #[test]
    fn bind_nested_placeholder_is_rejected() {
        let err = bind_params(
            &stream_template(),
            &params(&[
                ("owner", ColumnValue::param("evil")),
                ("min", ColumnValue::number(3)),
            ]),
        )
        .unwrap_err();
        assert_eq!(
            err,
            BindError::NestedParam {
                name: "owner".into()
            }
        );
    }

    #[test]
    fn bind_through_not_and_or() {
        let e = parse_predicate_expr("NOT (a = :x OR b = :y)").unwrap();
        let bound = bind_params(
            &e,
            &params(&[
                ("x", ColumnValue::number(1)),
                ("y", ColumnValue::boolean(true)),
            ]),
        )
        .unwrap();
        assert_eq!(
            bound,
            PredicateExpr::Not(Box::new(PredicateExpr::Or(vec![
                PredicateExpr::eq("a", ColumnValue::number(1)),
                PredicateExpr::eq("b", ColumnValue::boolean(true)),
            ])))
        );
    }

    #[test]
    fn a_fully_bound_tree_evaluates_against_rows() {
        // The structural guarantee behind Decision 2: after Ok(bind_params)
        // the tree holds only typed leaves and evaluates normally.
        let bound = bind_params(
            &stream_template(),
            &params(&[
                ("owner", ColumnValue::text("u1")),
                ("min", ColumnValue::number(3)),
            ]),
        )
        .unwrap();
        let yes = row(&[
            ("owner", ColumnValue::text("u1")),
            ("priority", ColumnValue::number(5)),
        ]);
        let wrong_owner = row(&[
            ("owner", ColumnValue::text("u2")),
            ("priority", ColumnValue::number(5)),
        ]);
        let too_low = row(&[
            ("owner", ColumnValue::text("u1")),
            ("priority", ColumnValue::number(1)),
        ]);
        assert!(bound.matches(yes));
        assert!(!bound.matches(wrong_owner));
        assert!(!bound.matches(too_low));
        // ...while the UNBOUND template matches nothing at all.
        let unbound = stream_template();
        assert!(!unbound.matches(row(&[
            ("owner", ColumnValue::text("u1")),
            ("priority", ColumnValue::number(5)),
        ])));
    }
}
