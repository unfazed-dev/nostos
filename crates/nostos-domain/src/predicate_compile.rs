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
//! literal    := NUMBER | FLOAT | "true" | "false" | "'TEXT'" | IDENT
//! OP         := "=" | "!=" | "<" | ">" | "<=" | ">="
//! ```
//!
//! Precedence (standard): `NOT` > `AND` > `OR`; parentheses override.
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

    /// literal := NUMBER | FLOAT | "'TEXT'" | IDENT
    /// (true/false were tokenized as Text("true"/"false") — but typed comparison
    /// needs a Bool. Detect them here and emit Bool; everything else is Text.)
    fn parse_literal(&mut self) -> Result<ColumnValue, ParseError> {
        match self.advance() {
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
}
