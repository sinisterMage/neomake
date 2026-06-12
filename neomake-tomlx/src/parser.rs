//! Recursive-descent parser for TOMLX expressions.
//!
//! Precedence (lowest to highest):
//!
//! ```text
//! or:      a || b
//! and:     a && b
//! equality: a == b | a != b
//! additive: a + b
//! unary:    !a | -a
//! primary:  literal | var | call | array | if-expr | ( expr )
//! ```
//!
//! The parser consumes the token stream produced by [`crate::lexer`]
//! and yields an [`Expr`] AST. String literals with `${...}` segments
//! are parsed here: each segment whose contents were captured as raw
//! source is re-lexed and re-parsed, giving us a proper AST.

use crate::error::TomlxError;
use crate::lexer::{tokenize, Span, SpannedToken, StrSegment, Token};

/// Binary operators recognized by TOMLX expressions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    /// `==`
    Eq,
    /// `!=`
    Ne,
    /// `&&`
    And,
    /// `||`
    Or,
    /// `+` (arithmetic or string concat — resolved at evaluation time)
    Plus,
}

/// Unary operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    /// `!`
    Not,
    /// `-`
    Neg,
}

/// A single template-string piece — either a literal fragment or a
/// fully-parsed embedded expression.
#[derive(Debug, Clone, PartialEq)]
pub enum StringPart {
    /// Literal text.
    Lit(String),
    /// An embedded expression whose value is stringified at evaluation time.
    Interp(Box<Expr>),
}

/// Abstract syntax tree node produced by the TOMLX parser.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// Integer literal.
    Int(i64),
    /// Floating-point literal.
    Float(f64),
    /// Boolean literal.
    Bool(bool),
    /// A single-quoted literal string (no interpolation).
    LitStr(String),
    /// A double-quoted string with zero or more interpolated segments.
    TemplateStr(Vec<StringPart>),
    /// An array literal `[a, b, c]`.
    Array(Vec<Expr>),
    /// `$name` or `${name}` — variable reference.
    Var(String),
    /// `ident(arg, ...)` — built-in function call.
    Call {
        /// Function name.
        name: String,
        /// Argument expressions.
        args: Vec<Expr>,
        /// Source location of the call (for diagnostics).
        span: Span,
    },
    /// Binary operation.
    Binary {
        /// Operator.
        op: BinOp,
        /// Left operand.
        lhs: Box<Expr>,
        /// Right operand.
        rhs: Box<Expr>,
    },
    /// Unary operation.
    Unary {
        /// Operator.
        op: UnaryOp,
        /// Operand.
        expr: Box<Expr>,
    },
    /// `if cond { then } else { else_ }` conditional expression.
    If {
        /// Boolean condition.
        cond: Box<Expr>,
        /// Expression when `cond` is true.
        then_branch: Box<Expr>,
        /// Expression when `cond` is false.
        else_branch: Box<Expr>,
    },
    /// `name = value` keyword argument inside a function call.
    KeywordArg {
        /// Parameter name.
        name: String,
        /// Parameter value.
        value: Box<Expr>,
    },
}

/// Parse a token stream into a single [`Expr`].
///
/// Fails if the stream is empty, has trailing tokens, or contains
/// syntax errors.
pub fn parse_expr(tokens: Vec<SpannedToken>) -> Result<Expr, TomlxError> {
    let mut p = Parser { tokens, pos: 0 };
    let expr = p.parse_expr()?;
    if let Some(tok) = p.tokens.get(p.pos) {
        return Err(TomlxError::new(
            &tok.span.file,
            tok.span.line,
            tok.span.col,
            format!("unexpected trailing token: {:?}", tok.token),
        ));
    }
    Ok(expr)
}

struct Parser {
    tokens: Vec<SpannedToken>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&SpannedToken> {
        self.tokens.get(self.pos)
    }

    fn bump(&mut self) -> Option<SpannedToken> {
        let t = self.tokens.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn eat(&mut self, kind: &Token) -> bool {
        if self
            .peek()
            .map(|t| discriminant_eq(&t.token, kind))
            .unwrap_or(false)
        {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, kind: &Token, what: &str) -> Result<SpannedToken, TomlxError> {
        match self.peek() {
            Some(t) if discriminant_eq(&t.token, kind) => {
                Ok(self.bump().expect("peeked token must exist"))
            }
            Some(t) => Err(TomlxError::new(
                &t.span.file,
                t.span.line,
                t.span.col,
                format!("expected {what}, got {:?}", t.token),
            )),
            None => Err(TomlxError::at_unknown(
                std::path::PathBuf::new(),
                format!("expected {what}, found end of input"),
            )),
        }
    }

    fn parse_expr(&mut self) -> Result<Expr, TomlxError> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> Result<Expr, TomlxError> {
        let mut lhs = self.parse_and()?;
        while self.eat(&Token::OrOr) {
            let rhs = self.parse_and()?;
            lhs = Expr::Binary {
                op: BinOp::Or,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> Result<Expr, TomlxError> {
        let mut lhs = self.parse_eq()?;
        while self.eat(&Token::AndAnd) {
            let rhs = self.parse_eq()?;
            lhs = Expr::Binary {
                op: BinOp::And,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn parse_eq(&mut self) -> Result<Expr, TomlxError> {
        let mut lhs = self.parse_add()?;
        loop {
            if self.eat(&Token::EqEq) {
                let rhs = self.parse_add()?;
                lhs = Expr::Binary {
                    op: BinOp::Eq,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                };
            } else if self.eat(&Token::NotEq) {
                let rhs = self.parse_add()?;
                lhs = Expr::Binary {
                    op: BinOp::Ne,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                };
            } else {
                break;
            }
        }
        Ok(lhs)
    }

    fn parse_add(&mut self) -> Result<Expr, TomlxError> {
        let mut lhs = self.parse_unary()?;
        while self.eat(&Token::Plus) {
            let rhs = self.parse_unary()?;
            lhs = Expr::Binary {
                op: BinOp::Plus,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> Result<Expr, TomlxError> {
        if self.eat(&Token::Bang) {
            let e = self.parse_unary()?;
            return Ok(Expr::Unary {
                op: UnaryOp::Not,
                expr: Box::new(e),
            });
        }
        if self.eat(&Token::Minus) {
            let e = self.parse_unary()?;
            return Ok(Expr::Unary {
                op: UnaryOp::Neg,
                expr: Box::new(e),
            });
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<Expr, TomlxError> {
        let tok = match self.bump() {
            Some(t) => t,
            None => {
                return Err(TomlxError::at_unknown(
                    std::path::PathBuf::new(),
                    "expected expression, found end of input",
                ))
            }
        };
        match tok.token {
            Token::Int(n) => Ok(Expr::Int(n)),
            Token::Float(f) => Ok(Expr::Float(f)),
            Token::Bool(b) => Ok(Expr::Bool(b)),
            Token::LitStr(s) => Ok(Expr::LitStr(s)),
            Token::TemplateStr(segs) => build_template(segs, &tok.span),
            Token::VarRef(name) => Ok(Expr::Var(name)),
            Token::LParen => {
                let inner = self.parse_expr()?;
                self.expect(&Token::RParen, "`)`")?;
                Ok(inner)
            }
            Token::LBracket => self.parse_array_tail(),
            Token::If => self.parse_if_tail(),
            Token::Ident(name) => {
                if self.eat(&Token::LParen) {
                    let mut args = Vec::new();
                    if !matches!(self.peek().map(|t| &t.token), Some(Token::RParen)) {
                        loop {
                            // lookahead: Ident Eq means keyword arg
                            let is_kwarg = matches!(
                                (self.tokens.get(self.pos), self.tokens.get(self.pos + 1)),
                                (
                                    Some(SpannedToken { token: Token::Ident(_), .. }),
                                    Some(SpannedToken { token: Token::Eq, .. })
                                )
                            );
                            if is_kwarg {
                                let kw_tok = self.bump().unwrap();
                                let kw_name = match kw_tok.token {
                                    Token::Ident(n) => n,
                                    _ => unreachable!(),
                                };
                                self.bump(); // eat Eq
                                let value = self.parse_expr()?;
                                args.push(Expr::KeywordArg {
                                    name: kw_name,
                                    value: Box::new(value),
                                });
                            } else {
                                args.push(self.parse_expr()?);
                            }
                            if !self.eat(&Token::Comma) {
                                break;
                            }
                        }
                    }
                    self.expect(&Token::RParen, "`)`")?;
                    Ok(Expr::Call {
                        name,
                        args,
                        span: tok.span,
                    })
                } else {
                    // Bare identifiers resolve as variable references.
                    // The `$` sigil is required at the *document* level
                    // to flag a TOMLX line, but once inside an
                    // expression (e.g. an `${...}` interpolation or the
                    // RHS of a `$var = ...` decl) the sigil is redundant.
                    Ok(Expr::Var(name))
                }
            }
            other => Err(TomlxError::new(
                &tok.span.file,
                tok.span.line,
                tok.span.col,
                format!("unexpected token: {other:?}"),
            )),
        }
    }

    fn parse_array_tail(&mut self) -> Result<Expr, TomlxError> {
        let mut items = Vec::new();
        if self.eat(&Token::RBracket) {
            return Ok(Expr::Array(items));
        }
        loop {
            items.push(self.parse_expr()?);
            if self.eat(&Token::Comma) {
                if self.eat(&Token::RBracket) {
                    return Ok(Expr::Array(items));
                }
                continue;
            }
            self.expect(&Token::RBracket, "`]` or `,`")?;
            return Ok(Expr::Array(items));
        }
    }

    fn parse_if_tail(&mut self) -> Result<Expr, TomlxError> {
        let cond = self.parse_expr()?;
        self.expect(&Token::LBrace, "`{` after if condition")?;
        let then_branch = self.parse_expr()?;
        self.expect(&Token::RBrace, "`}` to close if-then branch")?;
        self.expect(&Token::Else, "`else`")?;
        self.expect(&Token::LBrace, "`{` after else")?;
        let else_branch = self.parse_expr()?;
        self.expect(&Token::RBrace, "`}` to close else branch")?;
        Ok(Expr::If {
            cond: Box::new(cond),
            then_branch: Box::new(then_branch),
            else_branch: Box::new(else_branch),
        })
    }
}

fn build_template(segs: Vec<StrSegment>, span: &Span) -> Result<Expr, TomlxError> {
    let mut parts: Vec<StringPart> = Vec::with_capacity(segs.len());
    let mut has_interp = false;
    for seg in segs {
        match seg {
            StrSegment::Lit(s) => parts.push(StringPart::Lit(s)),
            StrSegment::Interp(raw) => {
                has_interp = true;
                let inner_tokens = tokenize(&raw, span)?;
                let inner = parse_expr(inner_tokens)?;
                parts.push(StringPart::Interp(Box::new(inner)));
            }
        }
    }
    if !has_interp {
        // Collapse to a single literal string for simpler evaluation.
        let mut buf = String::new();
        for p in parts {
            match p {
                StringPart::Lit(s) => buf.push_str(&s),
                StringPart::Interp(_) => unreachable!(),
            }
        }
        return Ok(Expr::LitStr(buf));
    }
    Ok(Expr::TemplateStr(parts))
}

fn discriminant_eq(a: &Token, b: &Token) -> bool {
    std::mem::discriminant(a) == std::mem::discriminant(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::{tokenize, Span};
    use std::path::PathBuf;

    fn parse(src: &str) -> Result<Expr, TomlxError> {
        let base = Span::new(PathBuf::from("t"), 1, 1);
        let toks = tokenize(src, &base)?;
        parse_expr(toks)
    }

    #[test]
    fn parses_precedence() {
        // `a || b && c == d` → a || (b && (c == d))
        let e = parse("1 == 2 && 3 == 4 || 5 == 6").unwrap();
        // Top-level must be Or.
        match e {
            Expr::Binary { op: BinOp::Or, .. } => {}
            other => panic!("expected top-level ||, got {other:?}"),
        }
    }

    #[test]
    fn parses_if_expr() {
        let e = parse("if true { 1 } else { 2 }").unwrap();
        assert!(matches!(e, Expr::If { .. }));
    }

    #[test]
    fn parses_call() {
        let e = parse(r#"env("X", "def")"#).unwrap();
        match e {
            Expr::Call { name, args, .. } => {
                assert_eq!(name, "env");
                assert_eq!(args.len(), 2);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parses_template_string() {
        let e = parse(r#""hello ${name}!""#).unwrap();
        match e {
            Expr::TemplateStr(parts) => {
                assert_eq!(parts.len(), 3);
                assert!(matches!(&parts[0], StringPart::Lit(s) if s == "hello "));
                assert!(matches!(&parts[1], StringPart::Interp(e) if matches!(**e, Expr::Var(_))));
                assert!(matches!(&parts[2], StringPart::Lit(s) if s == "!"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parses_array() {
        let e = parse(r#"["a", 1, true]"#).unwrap();
        match e {
            Expr::Array(items) => assert_eq!(items.len(), 3),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parses_keyword_args_in_call() {
        let e = parse(r#"git("repo", "tag", "dest", submodules = true, depth = 1)"#).unwrap();
        match e {
            Expr::Call { name, args, .. } => {
                assert_eq!(name, "git");
                assert_eq!(args.len(), 5);
                // first 3 are positional strings
                assert!(matches!(&args[0], Expr::LitStr(_)));
                assert!(matches!(&args[1], Expr::LitStr(_)));
                assert!(matches!(&args[2], Expr::LitStr(_)));
                // last 2 are keyword args
                match &args[3] {
                    Expr::KeywordArg { name, value } => {
                        assert_eq!(name, "submodules");
                        assert!(matches!(**value, Expr::Bool(true)));
                    }
                    other => panic!("expected KeywordArg, got {other:?}"),
                }
                match &args[4] {
                    Expr::KeywordArg { name, value } => {
                        assert_eq!(name, "depth");
                        assert!(matches!(**value, Expr::Int(1)));
                    }
                    other => panic!("expected KeywordArg, got {other:?}"),
                }
            }
            other => panic!("{other:?}"),
        }
    }
}
