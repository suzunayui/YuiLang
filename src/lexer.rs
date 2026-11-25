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
    Import,
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
    Pipe,
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
            Tok::Import => "import",
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
            Tok::Pipe => "|",
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
        "import" => Tok::Import,
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
    lex_full(src, base).map(|(toks, _)| toks)
}

/// Like `lex`, but also returns the span of every `//` comment.
pub fn lex_full(src: &str, base: usize) -> Result<(Vec<Token>, Vec<Span>), Diagnostic> {
    let b = src.as_bytes();
    let mut comments = Vec::new();
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
            let start = i;
            while i < b.len() && b[i] != b'\n' && b[i] != b'\r' {
                i += 1;
            }
            comments.push(Span::new(base + start, base + i));
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
        } else if c == b'\'' {
            // A character literal is an Int: the code of the character.
            let invalid = || {
                Diagnostic::new("E0006", "invalid character literal", Span::new(base + i, base + i + 1))
                    .hint("write one character between single quotes, e.g. 'a', '\\n' or '\\''")
            };
            let mut rest = src[i + 1..].chars();
            let (value, len) = match rest.next() {
                Some('\\') => {
                    let value = match rest.next() {
                        Some('n') => '\n',
                        Some('t') => '\t',
                        Some('r') => '\r',
                        Some('\\') => '\\',
                        Some('\'') => '\'',
                        Some('"') => '"',
                        _ => return Err(invalid()),
                    };
                    (value, 2)
                }
                Some(ch) if ch != '\'' && ch != '\n' => (ch, ch.len_utf8()),
                _ => return Err(invalid()),
            };
            if !src[i + 1 + len..].starts_with('\'') {
                return Err(invalid());
            }
            i += len + 2;
            Tok::Int(value as i64)
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
                        b'|' => Tok::Pipe,
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
    Ok((toks, comments))
}

/// The tokens of `src` as one line of text each, or up to the first error.
/// This is the format the lexer written in YuiLang (`selfhost/lexer.yui`)
/// prints, so that the two can be compared token by token.
///
/// A line starts with `+` when the token is the first on its source line,
/// otherwise with a space. Then: `int 42`, `float 1.5`, `ident name`,
/// `kw fn`, `sym ->`, `eof`, or `str` followed by tab-separated parts,
/// `L:text` for literal text and `C:code` for an interpolation.
pub fn dump(src: &str) -> Vec<String> {
    fn escape(s: &str) -> String {
        s.replace('\\', "\\\\").replace('\n', "\\n").replace('\t', "\\t").replace('\r', "\\r")
    }
    let (tokens, error) = match lex(src, 0) {
        Ok(tokens) => (tokens, None),
        Err(d) => (lex_prefix(src, d.span.start), Some(d.code)),
    };
    let mut lines = Vec::new();
    for t in &tokens {
        let mark = if t.nl_before { '+' } else { ' ' };
        let raw = &src[t.span.start..t.span.end];
        let body = match &t.tok {
            Tok::Int(_) if raw.starts_with('\'') => format!("int {raw}"),
            Tok::Int(_) => format!("int {}", raw.replace('_', "")),
            Tok::Float(_) => format!("float {}", raw.replace('_', "")),
            Tok::Ident(name) => format!("ident {name}"),
            Tok::Str(parts) => {
                let mut out = String::from("str");
                for part in parts {
                    match part {
                        StrPart::Lit(s) => out.push_str(&format!("\tL:{}", escape(s))),
                        StrPart::Code(code, _) => out.push_str(&format!("\tC:{}", escape(code))),
                    }
                }
                out
            }
            Tok::Eof => {
                if error.is_some() {
                    continue;
                }
                "eof".to_string()
            }
            other => {
                let text = other.text();
                let kind = if text.starts_with(|c: char| c.is_ascii_alphabetic()) { "kw" } else { "sym" };
                format!("{kind} {text}")
            }
        };
        lines.push(format!("{mark}{body}"));
    }
    if let Some(code) = error {
        lines.push(format!("error {code}"));
    }
    lines
}

/// The tokens that end before byte `end`: everything lexed before an error there.
fn lex_prefix(src: &str, end: usize) -> Vec<Token> {
    let mut cut = end.min(src.len());
    while !src.is_char_boundary(cut) {
        cut -= 1;
    }
    // The text before the error lexes cleanly unless it ends inside a token,
    // which only a string can do; then drop back to the start of that line.
    loop {
        match lex(&src[..cut], 0) {
            Ok(tokens) => return tokens,
            Err(d) if d.span.start < cut => cut = d.span.start,
            Err(_) => return Vec::new(),
        }
    }
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
