//! Lexer. Newlines are not tokens; each token records whether a newline precedes it.

use crate::diag::{Diagnostic, Span};

#[derive(Debug, Clone, PartialEq)]
pub enum StrPart {
    Lit(String),
    /// Source text of an interpolated expression and its absolute byte offset.
    Code(String, usize),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Int(i64),
    Float(f64),
    Str(Vec<StrPart>),
    Ident(String),
    Fn,
    Let,
    Mut,
    If,
    Else,
    While,
    For,
    In,
    Match,
    Return,
    Break,
    Continue,
    Struct,
    Enum,
    Test,
    Assert,
    Uses,
    Requires,
    Ensures,
    True,
    False,
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Comma,
    Colon,
    Dot,
    DotDot,
    Arrow,
    FatArrow,
    Eq,
    EqEq,
    NotEq,
    Lt,
    Le,
    Gt,
    Ge,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    AndAnd,
    OrOr,
    Bang,
    Question,
    Eof,
}

impl Tok {
    pub fn describe(&self) -> String {
        match self {
            Tok::Int(n) => format!("`{n}`"),
            Tok::Float(n) => format!("`{n}`"),
            Tok::Str(_) => "string literal".to_string(),
            Tok::Ident(s) => format!("`{s}`"),
            Tok::Eof => "end of file".to_string(),
            other => format!("`{}`", other.text()),
        }
    }

    fn text(&self) -> &'static str {
        match self {
            Tok::Fn => "fn",
            Tok::Let => "let",
            Tok::Mut => "mut",
            Tok::If => "if",
            Tok::Else => "else",
            Tok::While => "while",
            Tok::For => "for",
            Tok::In => "in",
            Tok::Match => "match",
            Tok::Return => "return",
            Tok::Break => "break",
            Tok::Continue => "continue",
            Tok::Struct => "struct",
            Tok::Enum => "enum",
            Tok::Test => "test",
            Tok::Assert => "assert",
            Tok::Uses => "uses",
            Tok::Requires => "requires",
            Tok::Ensures => "ensures",
            Tok::True => "true",
            Tok::False => "false",
            Tok::LParen => "(",
            Tok::RParen => ")",
            Tok::LBrace => "{",
            Tok::RBrace => "}",
            Tok::LBracket => "[",
            Tok::RBracket => "]",
            Tok::Comma => ",",
            Tok::Colon => ":",
            Tok::Dot => ".",
            Tok::DotDot => "..",
            Tok::Arrow => "->",
            Tok::FatArrow => "=>",
            Tok::Eq => "=",
            Tok::EqEq => "==",
            Tok::NotEq => "!=",
            Tok::Lt => "<",
            Tok::Le => "<=",
            Tok::Gt => ">",
            Tok::Ge => ">=",
            Tok::Plus => "+",
            Tok::Minus => "-",
            Tok::Star => "*",
            Tok::Slash => "/",
            Tok::Percent => "%",
            Tok::AndAnd => "&&",
            Tok::OrOr => "||",
            Tok::Bang => "!",
            Tok::Question => "?",
            _ => "?",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Token {
    pub tok: Tok,
    pub span: Span,
    pub nl_before: bool,
}

fn keyword(s: &str) -> Option<Tok> {
    Some(match s {
        "fn" => Tok::Fn,
        "let" => Tok::Let,
        "mut" => Tok::Mut,
        "if" => Tok::If,
        "else" => Tok::Else,
        "while" => Tok::While,
        "for" => Tok::For,
        "in" => Tok::In,
        "match" => Tok::Match,
        "return" => Tok::Return,
        "break" => Tok::Break,
        "continue" => Tok::Continue,
        "struct" => Tok::Struct,
        "enum" => Tok::Enum,
        "test" => Tok::Test,
        "assert" => Tok::Assert,
        "uses" => Tok::Uses,
        "requires" => Tok::Requires,
        "ensures" => Tok::Ensures,
        "true" => Tok::True,
        "false" => Tok::False,
        _ => return None,
    })
}

/// Lex `src`. `base` is added to every span so that interpolated code inside a
/// string literal reports positions in the enclosing file.
pub fn lex(src: &str, base: usize) -> Result<Vec<Token>, Diagnostic> {
    let b = src.as_bytes();
    let mut i = 0;
    let mut toks = Vec::new();
    let mut nl = true;

    while i < b.len() {
        let c = b[i];
        if c == b'\n' {
            nl = true;
            i += 1;
            continue;
        }
        if c == b' ' || c == b'\t' || c == b'\r' {
            i += 1;
            continue;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'/') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }

        let start = i;
        let tok = if c.is_ascii_digit() {
            while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'_') {
                i += 1;
            }
            let mut is_float = false;
            if i + 1 < b.len() && b[i] == b'.' && b[i + 1].is_ascii_digit() {
                is_float = true;
                i += 1;
                while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'_') {
                    i += 1;
                }
            }
            let text: String = src[start..i].chars().filter(|&ch| ch != '_').collect();
            let span = Span::new(base + start, base + i);
            if is_float {
                match text.parse::<f64>() {
                    Ok(v) => Tok::Float(v),
                    Err(_) => return Err(Diagnostic::new("E0003", "invalid float literal", span)),
                }
            } else {
                match text.parse::<i64>() {
                    Ok(v) => Tok::Int(v),
                    Err(_) => {
                        return Err(Diagnostic::new(
                            "E0003",
                            "integer literal does not fit in Int (64-bit signed)",
                            span,
                        ))
                    }
                }
            }
        } else if c.is_ascii_alphabetic() || c == b'_' {
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            let text = &src[start..i];
            keyword(text).unwrap_or_else(|| Tok::Ident(text.to_string()))
        } else if c == b'"' {
            let (parts, next) = lex_string(src, i, base)?;
            i = next;
            Tok::Str(parts)
        } else {
            let two = if i + 1 < b.len() { &b[i..i + 2] } else { &b[i..i + 1] };
            let (tok, len) = match two {
                b".." => (Tok::DotDot, 2),
                b"->" => (Tok::Arrow, 2),
                b"=>" => (Tok::FatArrow, 2),
                b"==" => (Tok::EqEq, 2),
                b"!=" => (Tok::NotEq, 2),
                b"<=" => (Tok::Le, 2),
                b">=" => (Tok::Ge, 2),
                b"&&" => (Tok::AndAnd, 2),
                b"||" => (Tok::OrOr, 2),
                _ => {
                    let t = match c {
                        b'(' => Tok::LParen,
                        b')' => Tok::RParen,
                        b'{' => Tok::LBrace,
                        b'}' => Tok::RBrace,
                        b'[' => Tok::LBracket,
                        b']' => Tok::RBracket,
                        b',' => Tok::Comma,
                        b':' => Tok::Colon,
                        b'.' => Tok::Dot,
                        b'=' => Tok::Eq,
                        b'<' => Tok::Lt,
                        b'>' => Tok::Gt,
                        b'+' => Tok::Plus,
                        b'-' => Tok::Minus,
                        b'*' => Tok::Star,
                        b'/' => Tok::Slash,
                        b'%' => Tok::Percent,
                        b'!' => Tok::Bang,
                        b'?' => Tok::Question,
                        _ => {
                            let ch = src[i..].chars().next().unwrap();
                            let span = Span::new(base + i, base + i + ch.len_utf8());
                            let mut d = Diagnostic::new(
                                "E0001",
                                format!("unexpected character `{ch}`"),
                                span,
                            );
                            if ch == ';' {
                                d = d.hint("YuiLang has no semicolons; put each statement on its own line");
                            } else if !ch.is_ascii() {
                                d = d.hint("non-ASCII characters are only allowed in strings and comments");
                            }
                            return Err(d);
                        }
                    };
                    (t, 1)
                }
            };
            i += len;
            tok
        };

        toks.push(Token { tok, span: Span::new(base + start, base + i), nl_before: nl });
        nl = false;
    }

    toks.push(Token { tok: Tok::Eof, span: Span::new(base + b.len(), base + b.len()), nl_before: nl });
    Ok(toks)
}

/// Lex a string literal starting at the opening quote. Returns its parts and
/// the index just past the closing quote.
fn lex_string(src: &str, open: usize, base: usize) -> Result<(Vec<StrPart>, usize), Diagnostic> {
    let b = src.as_bytes();
    let unterminated = |at: usize| {
        Diagnostic::new("E0002", "unterminated string literal", Span::new(base + open, base + at))
            .hint("strings must close with `\"` on the same line; use `\\n` for a line break")
    };
    let mut i = open + 1;
    let mut parts = Vec::new();
    let mut cur = String::new();
    loop {
        if i >= b.len() {
            return Err(unterminated(i));
        }
        let ch = src[i..].chars().next().unwrap();
        match ch {
            '"' => {
                i += 1;
                break;
            }
            '\n' => return Err(unterminated(i)),
            '\\' => {
                let esc = match b.get(i + 1) {
                    Some(b'n') => '\n',
                    Some(b't') => '\t',
                    Some(b'r') => '\r',
                    Some(b'\\') => '\\',
                    Some(b'"') => '"',
                    Some(b'{') => '{',
                    Some(b'}') => '}',
                    _ => {
                        return Err(Diagnostic::new(
                            "E0004",
                            "invalid escape sequence",
                            Span::new(base + i, base + (i + 2).min(b.len())),
                        )
                        .hint("valid escapes are \\n \\t \\r \\\\ \\\" \\{ \\}"))
                    }
                };
                cur.push(esc);
                i += 2;
            }
            '{' => {
                let code_start = i + 1;
                let mut depth = 1;
                let mut j = code_start;
                loop {
                    if j >= b.len() || b[j] == b'\n' {
                        return Err(unterminated(j));
                    }
                    match b[j] {
                        b'{' => depth += 1,
                        b'}' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        b'"' => {
                            return Err(Diagnostic::new(
                                "E0005",
                                "string literals are not allowed inside `{...}` interpolation",
                                Span::new(base + j, base + j + 1),
                            )
                            .hint("bind the value with `let` first, or write `\\{` for a literal brace"))
                        }
                        _ => {}
                    }
                    j += 1;
                }
                if !cur.is_empty() {
                    parts.push(StrPart::Lit(std::mem::take(&mut cur)));
                }
                parts.push(StrPart::Code(src[code_start..j].to_string(), base + code_start));
                i = j + 1;
            }
            _ => {
                cur.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    if !cur.is_empty() || parts.is_empty() {
        parts.push(StrPart::Lit(cur));
    }
    Ok((parts, i))
}
