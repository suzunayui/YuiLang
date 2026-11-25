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

pub struct SourceFile {
    pub name: String,
    pub start: usize,
    pub end: usize,
}

/// All loaded source files, concatenated into one buffer so that a `Span`
/// identifies both the file and the position inside it.
#[derive(Default)]
pub struct SourceMap {
    pub text: String,
    pub files: Vec<SourceFile>,
}

impl SourceMap {
    pub fn single(name: &str, src: &str) -> SourceMap {
        let mut map = SourceMap::default();
        map.add(name, src);
        map
    }

    /// Adds a file and returns the offset of its first byte.
    pub fn add(&mut self, name: &str, src: &str) -> usize {
        let start = self.text.len();
        self.text.push_str(src);
        let end = self.text.len();
        self.text.push('\n');
        self.files.push(SourceFile { name: name.replace('\\', "/"), start, end });
        start
    }

    fn file_of(&self, offset: usize) -> Option<&SourceFile> {
        self.files.iter().rev().find(|f| f.start <= offset).or(self.files.first())
    }

    /// File name and 1-based (line, column) of an offset. Columns count characters.
    pub fn locate(&self, offset: usize) -> (&str, usize, usize) {
        let Some(file) = self.file_of(offset) else { return ("<unknown>", 1, 1) };
        let offset = offset.clamp(file.start, file.end);
        let mut line = 1;
        let mut col = 1;
        for ch in self.text[file.start..offset].chars() {
            if ch == '\n' {
                line += 1;
                col = 1;
            } else {
                col += 1;
            }
        }
        (&file.name, line, col)
    }

    fn line_text(&self, offset: usize) -> &str {
        let Some(file) = self.file_of(offset) else { return "" };
        let offset = offset.clamp(file.start, file.end);
        let text = &self.text[file.start..file.end];
        let local = offset - file.start;
        let begin = text[..local].rfind('\n').map_or(0, |i| i + 1);
        let end = text[local..].find('\n').map_or(text.len(), |i| local + i);
        text[begin..end].trim_end_matches('\r')
    }
}

pub fn render_human(d: &Diagnostic, sources: &SourceMap) -> String {
    let (file, line, col) = sources.locate(d.span.start);
    let (_, end_line, end_col) = sources.locate(d.span.end);
    let text = sources.line_text(d.span.start);
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

pub fn render_json(d: &Diagnostic, sources: &SourceMap) -> String {
    let (file, line, col) = sources.locate(d.span.start);
    let (_, end_line, end_col) = sources.locate(d.span.end);
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
