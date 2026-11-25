//! Diagnostics: every error carries a stable code, a span and an optional hint,
//! and can be rendered for humans or as JSON.

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Span {
        Span { start, end }
    }

    pub fn to(self, other: Span) -> Span {
        Span { start: self.start.min(other.start), end: self.end.max(other.end) }
    }
}

#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub code: &'static str,
    pub message: String,
    pub span: Span,
    pub hint: Option<String>,
}

impl Diagnostic {
    pub fn new(code: &'static str, message: impl Into<String>, span: Span) -> Diagnostic {
        Diagnostic { code, message: message.into(), span, hint: None }
    }

    pub fn hint(mut self, hint: impl Into<String>) -> Diagnostic {
        self.hint = Some(hint.into());
        self
    }
}

/// 1-based (line, column) of a byte offset. Columns count characters.
pub fn line_col(src: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(src.len());
    let mut line = 1;
    let mut col = 1;
    for (i, ch) in src.char_indices() {
        if i >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

pub fn render_human(d: &Diagnostic, file: &str, src: &str) -> String {
    let (line, col) = line_col(src, d.span.start);
    let (end_line, end_col) = line_col(src, d.span.end);
    let text = src.lines().nth(line - 1).unwrap_or("");
    let width = if end_line == line && end_col > col { end_col - col } else { 1 };
    let num = line.to_string();
    let pad = " ".repeat(num.len());
    let mut out = String::new();
    out.push_str(&format!("error[{}]: {}\n", d.code, d.message));
    out.push_str(&format!("{pad}--> {file}:{line}:{col}\n"));
    out.push_str(&format!("{pad} |\n"));
    out.push_str(&format!("{num} | {}\n", text.replace('\t', " ")));
    out.push_str(&format!("{pad} | {}{}\n", " ".repeat(col - 1), "^".repeat(width)));
    if let Some(h) = &d.hint {
        out.push_str(&format!("{pad} = hint: {h}\n"));
    }
    out
}

pub fn render_json(d: &Diagnostic, file: &str, src: &str) -> String {
    let (line, col) = line_col(src, d.span.start);
    let (end_line, end_col) = line_col(src, d.span.end);
    let hint = match &d.hint {
        Some(h) => json_str(h),
        None => "null".to_string(),
    };
    format!(
        "{{\"code\":{},\"message\":{},\"file\":{},\"line\":{},\"column\":{},\"end_line\":{},\"end_column\":{},\"hint\":{}}}",
        json_str(d.code),
        json_str(&d.message),
        json_str(file),
        line,
        col,
        end_line,
        end_col,
        hint
    )
}

pub fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
