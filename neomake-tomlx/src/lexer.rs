//! Expression-level lexer for TOMLX.
//!
//! The lexer operates on a *single TOMLX expression slice* — for
//! example, the right-hand side of `$var = <expr>` or of a
//! `key = <expr>` line that was flagged as containing TOMLX syntax by
//! the document scanner (see `evaluator.rs`). It is deliberately not a
//! full TOML lexer: the document scanner keeps most of the file as
//! verbatim TOML and only sends us the pieces that need evaluation.
//!
//! Every token carries a [`Span`] with file, line, and column so that
//! diagnostics can be reported against the original source (even though
//! the lexer may have been invoked on a sub-slice).

use std::path::{Path, PathBuf};

use crate::error::TomlxError;

/// A location in the source, used for diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    /// Source file path.
    pub file: PathBuf,
    /// 1-indexed line number.
    pub line: usize,
    /// 1-indexed column number.
    pub col: usize,
}

impl Span {
    /// Construct a new span.
    pub fn new(file: impl Into<PathBuf>, line: usize, col: usize) -> Self {
        Self {
            file: file.into(),
            line,
            col,
        }
    }
}

/// Tokens emitted by the TOMLX expression lexer.
#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    /// A bare identifier (function name, keyword context).
    Ident(String),
    /// Integer literal (may be negative).
    Int(i64),
    /// Floating-point literal.
    Float(f64),
    /// Boolean literal.
    Bool(bool),
    /// Double-quoted (template) string — possibly containing `${...}`
    /// interpolations. Each segment is either a literal chunk or a raw
    /// source slice of an expression.
    TemplateStr(Vec<StrSegment>),
    /// Single-quoted (literal) string — no interpolation, no escapes.
    LitStr(String),
    /// `$IDENT` or `${IDENT}` — a bare variable reference.
    VarRef(String),
    /// `if` keyword.
    If,
    /// `else` keyword.
    Else,
    /// `(`
    LParen,
    /// `)`
    RParen,
    /// `{`
    LBrace,
    /// `}`
    RBrace,
    /// `[`
    LBracket,
    /// `]`
    RBracket,
    /// `,`
    Comma,
    /// `+`
    Plus,
    /// `-`
    Minus,
    /// `==`
    EqEq,
    /// `!=`
    NotEq,
    /// `&&`
    AndAnd,
    /// `||`
    OrOr,
    /// `!`
    Bang,
    /// `=` (bare equals, for keyword args in function calls)
    Eq,
}

/// A segment inside a template (double-quoted) string.
#[derive(Debug, Clone, PartialEq)]
pub enum StrSegment {
    /// A literal, fully-unescaped character sequence.
    Lit(String),
    /// Raw source of an expression found between `${` and the matching `}`.
    /// The parser re-lexes and re-parses this slice.
    Interp(String),
}

/// A token paired with its source location.
#[derive(Debug, Clone)]
pub struct SpannedToken {
    /// Token payload.
    pub token: Token,
    /// Location where the token starts.
    pub span: Span,
}

/// Tokenize `src` as a single TOMLX expression.
///
/// `base` is the location of `src[0]` in the original file; the lexer
/// tracks newlines and columns relative to that base so that reported
/// diagnostics match the user's source.
pub fn tokenize(src: &str, base: &Span) -> Result<Vec<SpannedToken>, TomlxError> {
    let mut lex = Lexer::new(src, base);
    let mut out = Vec::new();
    while let Some(tok) = lex.next_token()? {
        out.push(tok);
    }
    Ok(out)
}

struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
    file: PathBuf,
    line: usize,
    col: usize,
}

impl<'a> Lexer<'a> {
    fn new(src: &'a str, base: &Span) -> Self {
        Self {
            src: src.as_bytes(),
            pos: 0,
            file: base.file.clone(),
            line: base.line,
            col: base.col,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn peek_at(&self, off: usize) -> Option<u8> {
        self.src.get(self.pos + off).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.pos += 1;
        if b == b'\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        Some(b)
    }

    fn span(&self) -> Span {
        Span::new(self.file.clone(), self.line, self.col)
    }

    fn err(&self, msg: impl Into<String>) -> TomlxError {
        TomlxError::new(&self.file, self.line, self.col, msg)
    }

    fn skip_ws_and_comments(&mut self) {
        loop {
            match self.peek() {
                Some(b) if b == b' ' || b == b'\t' || b == b'\n' || b == b'\r' => {
                    self.bump();
                }
                Some(b'#') => {
                    while let Some(b) = self.peek() {
                        if b == b'\n' {
                            break;
                        }
                        self.bump();
                    }
                }
                _ => break,
            }
        }
    }

    fn next_token(&mut self) -> Result<Option<SpannedToken>, TomlxError> {
        self.skip_ws_and_comments();
        let span = self.span();
        let b = match self.peek() {
            Some(b) => b,
            None => return Ok(None),
        };

        // Operators & punctuation.
        match b {
            b'(' => {
                self.bump();
                return Ok(Some(SpannedToken {
                    token: Token::LParen,
                    span,
                }));
            }
            b')' => {
                self.bump();
                return Ok(Some(SpannedToken {
                    token: Token::RParen,
                    span,
                }));
            }
            b'{' => {
                self.bump();
                return Ok(Some(SpannedToken {
                    token: Token::LBrace,
                    span,
                }));
            }
            b'}' => {
                self.bump();
                return Ok(Some(SpannedToken {
                    token: Token::RBrace,
                    span,
                }));
            }
            b'[' => {
                self.bump();
                return Ok(Some(SpannedToken {
                    token: Token::LBracket,
                    span,
                }));
            }
            b']' => {
                self.bump();
                return Ok(Some(SpannedToken {
                    token: Token::RBracket,
                    span,
                }));
            }
            b',' => {
                self.bump();
                return Ok(Some(SpannedToken {
                    token: Token::Comma,
                    span,
                }));
            }
            b'+' => {
                self.bump();
                return Ok(Some(SpannedToken {
                    token: Token::Plus,
                    span,
                }));
            }
            b'-' => {
                // Could be unary or start of a number. Emit Minus; the
                // parser decides.
                self.bump();
                return Ok(Some(SpannedToken {
                    token: Token::Minus,
                    span,
                }));
            }
            b'=' => {
                if self.peek_at(1) == Some(b'=') {
                    self.bump();
                    self.bump();
                    return Ok(Some(SpannedToken {
                        token: Token::EqEq,
                        span,
                    }));
                }
                // bare = for keyword args like `depth = 1`
                self.bump();
                return Ok(Some(SpannedToken {
                    token: Token::Eq,
                    span,
                }));
            }
            b'!' => {
                if self.peek_at(1) == Some(b'=') {
                    self.bump();
                    self.bump();
                    return Ok(Some(SpannedToken {
                        token: Token::NotEq,
                        span,
                    }));
                }
                self.bump();
                return Ok(Some(SpannedToken {
                    token: Token::Bang,
                    span,
                }));
            }
            b'&' => {
                if self.peek_at(1) == Some(b'&') {
                    self.bump();
                    self.bump();
                    return Ok(Some(SpannedToken {
                        token: Token::AndAnd,
                        span,
                    }));
                }
                return Err(self.err("unexpected `&` (did you mean `&&`?)"));
            }
            b'|' => {
                if self.peek_at(1) == Some(b'|') {
                    self.bump();
                    self.bump();
                    return Ok(Some(SpannedToken {
                        token: Token::OrOr,
                        span,
                    }));
                }
                return Err(self.err("unexpected `|` (did you mean `||`?)"));
            }
            _ => {}
        }

        // Variable references.
        if b == b'$' {
            self.bump();
            if self.peek() == Some(b'{') {
                self.bump();
                let name = self.read_ident()?;
                if self.peek() != Some(b'}') {
                    return Err(self.err("expected `}` to close `${...}`"));
                }
                self.bump();
                return Ok(Some(SpannedToken {
                    token: Token::VarRef(name),
                    span,
                }));
            }
            let name = self.read_ident()?;
            return Ok(Some(SpannedToken {
                token: Token::VarRef(name),
                span,
            }));
        }

        // Strings.
        if b == b'"' {
            let segments = self.read_template_string()?;
            return Ok(Some(SpannedToken {
                token: Token::TemplateStr(segments),
                span,
            }));
        }
        if b == b'\'' {
            let s = self.read_literal_string()?;
            return Ok(Some(SpannedToken {
                token: Token::LitStr(s),
                span,
            }));
        }

        // Numbers.
        if b.is_ascii_digit() {
            return self.read_number(span).map(Some);
        }

        // Identifiers / keywords.
        if is_ident_start(b) {
            let ident = self.read_ident()?;
            let tok = match ident.as_str() {
                "true" => Token::Bool(true),
                "false" => Token::Bool(false),
                "if" => Token::If,
                "else" => Token::Else,
                _ => Token::Ident(ident),
            };
            return Ok(Some(SpannedToken { token: tok, span }));
        }

        Err(self.err(format!("unexpected character `{}`", b as char)))
    }

    fn read_ident(&mut self) -> Result<String, TomlxError> {
        let start = self.pos;
        if !self.peek().map(is_ident_start).unwrap_or(false) {
            return Err(self.err("expected identifier"));
        }
        self.bump();
        while let Some(b) = self.peek() {
            if is_ident_continue(b) {
                self.bump();
            } else {
                break;
            }
        }
        let bytes = &self.src[start..self.pos];
        std::str::from_utf8(bytes)
            .map(|s| s.to_string())
            .map_err(|_| self.err("invalid UTF-8 in identifier"))
    }

    fn read_number(&mut self, span: Span) -> Result<SpannedToken, TomlxError> {
        let start = self.pos;
        while let Some(b) = self.peek() {
            if b.is_ascii_digit() {
                self.bump();
            } else {
                break;
            }
        }
        let mut is_float = false;
        if self.peek() == Some(b'.') && self.peek_at(1).map(|b| b.is_ascii_digit()).unwrap_or(false)
        {
            is_float = true;
            self.bump();
            while let Some(b) = self.peek() {
                if b.is_ascii_digit() {
                    self.bump();
                } else {
                    break;
                }
            }
        }
        let text = std::str::from_utf8(&self.src[start..self.pos])
            .map_err(|_| self.err("invalid UTF-8 in number"))?;
        let token = if is_float {
            Token::Float(
                text.parse::<f64>()
                    .map_err(|_| self.err(format!("invalid float `{text}`")))?,
            )
        } else {
            Token::Int(
                text.parse::<i64>()
                    .map_err(|_| self.err(format!("invalid integer `{text}`")))?,
            )
        };
        Ok(SpannedToken { token, span })
    }

    fn read_template_string(&mut self) -> Result<Vec<StrSegment>, TomlxError> {
        // Consume opening `"`.
        self.bump();
        let mut segments: Vec<StrSegment> = Vec::new();
        let mut buf = String::new();

        loop {
            let b = match self.peek() {
                Some(b) => b,
                None => return Err(self.err("unterminated string")),
            };
            match b {
                b'"' => {
                    self.bump();
                    if !buf.is_empty() || segments.is_empty() {
                        segments.push(StrSegment::Lit(std::mem::take(&mut buf)));
                    }
                    return Ok(segments);
                }
                b'\\' => {
                    self.bump();
                    let esc = self.bump().ok_or_else(|| self.err("unterminated escape"))?;
                    match esc {
                        b'n' => buf.push('\n'),
                        b't' => buf.push('\t'),
                        b'r' => buf.push('\r'),
                        b'\\' => buf.push('\\'),
                        b'"' => buf.push('"'),
                        b'$' => buf.push('$'),
                        b'0' => buf.push('\0'),
                        other => {
                            return Err(self.err(format!("unknown escape `\\{}`", other as char)))
                        }
                    }
                }
                b'$' if self.peek_at(1) == Some(b'{') => {
                    if !buf.is_empty() {
                        segments.push(StrSegment::Lit(std::mem::take(&mut buf)));
                    }
                    self.bump();
                    self.bump();
                    let mut depth = 1usize;
                    let interp_start = self.pos;
                    while depth > 0 {
                        let c = match self.peek() {
                            Some(c) => c,
                            None => {
                                return Err(
                                    self.err("unterminated `${...}` interpolation in string")
                                )
                            }
                        };
                        match c {
                            b'{' => {
                                depth += 1;
                                self.bump();
                            }
                            b'}' => {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                                self.bump();
                            }
                            b'"' => {
                                // Nested strings inside interpolations — consume to closing quote.
                                self.bump();
                                while let Some(c2) = self.peek() {
                                    if c2 == b'\\' {
                                        self.bump();
                                        self.bump();
                                    } else if c2 == b'"' {
                                        self.bump();
                                        break;
                                    } else {
                                        self.bump();
                                    }
                                }
                            }
                            _ => {
                                self.bump();
                            }
                        }
                    }
                    let raw = std::str::from_utf8(&self.src[interp_start..self.pos])
                        .map_err(|_| self.err("invalid UTF-8 in interpolation"))?
                        .to_string();
                    segments.push(StrSegment::Interp(raw));
                    self.bump(); // consume closing `}`
                }
                _ => {
                    buf.push(b as char);
                    self.bump();
                }
            }
        }
    }

    fn read_literal_string(&mut self) -> Result<String, TomlxError> {
        self.bump(); // opening `'`
        let start = self.pos;
        loop {
            let b = match self.peek() {
                Some(b) => b,
                None => return Err(self.err("unterminated literal string")),
            };
            if b == b'\'' {
                let bytes = &self.src[start..self.pos];
                let s = std::str::from_utf8(bytes)
                    .map_err(|_| self.err("invalid UTF-8 in literal string"))?
                    .to_string();
                self.bump();
                return Ok(s);
            }
            if b == b'\n' {
                return Err(self.err("literal strings cannot contain newlines"));
            }
            self.bump();
        }
    }
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// Convenience: construct a span rooted at the top of a file.
pub fn span_for(file: &Path) -> Span {
    Span::new(file, 1, 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn base() -> Span {
        Span::new(PathBuf::from("test"), 1, 1)
    }

    fn tokens(src: &str) -> Vec<Token> {
        tokenize(src, &base())
            .unwrap()
            .into_iter()
            .map(|st| st.token)
            .collect()
    }

    #[test]
    fn lexes_literals_and_operators() {
        let toks = tokens("1 + 2 == 3 && true || false");
        assert_eq!(
            toks,
            vec![
                Token::Int(1),
                Token::Plus,
                Token::Int(2),
                Token::EqEq,
                Token::Int(3),
                Token::AndAnd,
                Token::Bool(true),
                Token::OrOr,
                Token::Bool(false),
            ]
        );
    }

    #[test]
    fn lexes_var_refs() {
        let toks = tokens("$foo ${bar_baz}");
        assert_eq!(
            toks,
            vec![Token::VarRef("foo".into()), Token::VarRef("bar_baz".into()),]
        );
    }

    #[test]
    fn lexes_template_string_with_interp() {
        let toks = tokens(r#""hello ${name} x""#);
        match &toks[0] {
            Token::TemplateStr(segs) => {
                assert_eq!(segs.len(), 3);
                assert!(matches!(&segs[0], StrSegment::Lit(s) if s == "hello "));
                assert!(matches!(&segs[1], StrSegment::Interp(s) if s == "name"));
                assert!(matches!(&segs[2], StrSegment::Lit(s) if s == " x"));
            }
            other => panic!("expected TemplateStr, got {other:?}"),
        }
    }

    #[test]
    fn lexes_literal_string() {
        let toks = tokens("'hi ${not interpolated}'");
        match &toks[0] {
            Token::LitStr(s) => assert_eq!(s, "hi ${not interpolated}"),
            other => panic!("expected LitStr, got {other:?}"),
        }
    }

    #[test]
    fn lexes_if_else_and_call() {
        let toks = tokens("if env(\"X\") == \"1\" { 1 } else { 2 }");
        assert!(toks.contains(&Token::If));
        assert!(toks.contains(&Token::Else));
        assert!(toks.contains(&Token::EqEq));
        assert!(toks
            .iter()
            .any(|t| matches!(t, Token::Ident(s) if s == "env")));
    }

    #[test]
    fn lexes_bare_eq_for_kwargs() {
        let toks = tokens("depth = 1");
        assert_eq!(
            toks,
            vec![Token::Ident("depth".into()), Token::Eq, Token::Int(1)]
        );
    }

    #[test]
    fn eq_eq_still_works() {
        let toks = tokens("a == b");
        assert!(toks.contains(&Token::EqEq));
    }
}
