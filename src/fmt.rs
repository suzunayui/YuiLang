//! Source formatter: one canonical layout for every program.
//!
//! The formatter prints the syntax tree and re-attaches comments at statement
//! boundaries. A construct is kept on one line when it fits within `WIDTH`
//! columns and contains no comments; otherwise it is broken in a fixed way.

use crate::ast::*;
use crate::diag::{Diagnostic, Span};
use crate::{lexer, parser};

const WIDTH: usize = 100;

pub fn format_source(src: &str) -> Result<String, Diagnostic> {
    let src = src.replace("\r\n", "\n");
    let (_, comments) = lexer::lex_full(&src, 0)?;
    let program = parser::parse_file(&src, 0, "", false)?;
    let count = comments.len();
    let mut f = Formatter { src: &src, comments, next: 0 };
    let out = f.program(&program);

    // Never hand back output that lost a comment or no longer parses.
    let internal = |what: &str| {
        Diagnostic::new("E0901", format!("internal formatter error: {what}"), Span::new(0, 0))
            .hint("the file was left unchanged; please report this")
    };
    match lexer::lex_full(&out, 0) {
        Ok((_, after)) if after.len() == count => {}
        _ => return Err(internal("a comment was lost")),
    }
    if parser::parse_file(&out, 0, "", false).is_err() {
        return Err(internal("the formatted output does not parse"));
    }
    Ok(out)
}

struct Formatter<'a> {
    src: &'a str,
    comments: Vec<Span>,
    /// Index of the next comment that has not been printed yet.
    next: usize,
}

fn ind(level: usize) -> String {
    "  ".repeat(level)
}

fn binprec(op: BinOp) -> u8 {
    match op {
        BinOp::Or => 1,
        BinOp::And => 2,
        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => 3,
        BinOp::Add | BinOp::Sub => 4,
        BinOp::Mul | BinOp::Div | BinOp::Rem => 5,
    }
}

fn prec(e: &Expr) -> u8 {
    match &e.kind {
        ExprKind::Binary(op, _, _) => binprec(*op),
        ExprKind::Unary(..) => 6,
        ExprKind::If { .. }
        | ExprKind::Match { .. }
        | ExprKind::Block(_)
        | ExprKind::Closure { .. }
        | ExprKind::Return(_)
        | ExprKind::Break
        | ExprKind::Continue => 0,
        _ => 8,
    }
}

fn unop(op: UnOp) -> &'static str {
    match op {
        UnOp::Neg => "-",
        UnOp::Not => "!",
    }
}

fn ctor_name(enum_name: &str, variant: &str) -> String {
    match enum_name {
        "Option" | "Result" => variant.to_string(),
        _ => format!("{enum_name}.{variant}"),
    }
}

fn effects(effects: &[(String, Span)]) -> String {
    if effects.is_empty() {
        String::new()
    } else {
        let names: Vec<&str> = effects.iter().map(|(e, _)| e.as_str()).collect();
        format!(" uses {}", names.join(", "))
    }
}

fn generics(params: &[String]) -> String {
    if params.is_empty() {
        String::new()
    } else {
        format!("<{}>", params.join(", "))
    }
}

enum Label<'e> {
    None,
    Field(&'e str),
    Key(&'e Expr),
}

/// A bracketed, comma-separated construct: call, list, struct literal, ...
struct Seq<'e> {
    recv: Option<&'e Expr>,
    open: String,
    close: &'static str,
    /// Whether the single-line form has spaces inside the brackets.
    pad: bool,
    items: Vec<(Label<'e>, &'e Expr)>,
}

fn seq(e: &Expr) -> Option<Seq<'_>> {
    fn plain(args: &[Expr]) -> Vec<(Label<'_>, &Expr)> {
        args.iter().map(|a| (Label::None, a)).collect()
    }
    Some(match &e.kind {
        ExprKind::Call { name, args, .. } => {
            Seq { recv: None, open: format!("{name}("), close: ")", pad: false, items: plain(args) }
        }
        ExprKind::MethodCall { recv, name, args, .. } => {
            Seq { recv: Some(recv), open: format!(".{name}("), close: ")", pad: false, items: plain(args) }
        }
        ExprKind::List(items) => Seq { recv: None, open: "[".into(), close: "]", pad: false, items: plain(items) },
        ExprKind::Ctor { enum_name, variant, args } if !args.is_empty() => Seq {
            recv: None,
            open: format!("{}(", ctor_name(enum_name, variant)),
            close: ")",
            pad: false,
            items: plain(args),
        },
        ExprKind::StructLit { name, fields } => Seq {
            recv: None,
            open: format!("{name} {{"),
            close: "}",
            pad: true,
            items: fields.iter().map(|(n, _, v)| (Label::Field(n.as_str()), v)).collect(),
        },
        ExprKind::MapLit(entries) => Seq {
            recv: None,
            open: "Map {".into(),
            close: "}",
            pad: true,
            items: entries.iter().map(|(k, v)| (Label::Key(k), v)).collect(),
        },
        _ => return None,
    })
}

impl<'a> Formatter<'a> {
    fn text(&self, span: Span) -> &str {
        &self.src[span.start..span.end]
    }

    // ---- comments ----

    fn has_comment(&self, span: Span) -> bool {
        self.comments[self.next..].iter().any(|c| c.start >= span.start && c.start < span.end)
    }

    /// Whether an empty line precedes the line that `pos` is on.
    fn blank_before(&self, pos: usize) -> bool {
        let mut newlines = 0;
        for ch in self.src[..pos].chars().rev() {
            match ch {
                ' ' | '\t' => {}
                '\n' => newlines += 1,
                _ => break,
            }
        }
        newlines >= 2
    }

    /// Prints pending comments that start before `pos`, one per line.
    /// `first` suppresses a blank line before the first comment.
    fn leading(&mut self, pos: usize, level: usize, out: &mut String, first: bool) -> bool {
        let mut any = false;
        while self.next < self.comments.len() && self.comments[self.next].start < pos {
            let c = self.comments[self.next];
            if (any || !first) && self.blank_before(c.start) {
                out.push('\n');
            }
            out.push_str(&ind(level));
            out.push_str(self.text(c).trim_end());
            out.push('\n');
            self.next += 1;
            any = true;
        }
        any
    }

    /// Comments and blank line before an element of a block-like sequence.
    fn before(&mut self, pos: usize, level: usize, out: &mut String, first: bool) {
        let any = self.leading(pos, level, out, first);
        if (any || !first) && self.blank_before(pos) {
            out.push('\n');
        }
    }

    /// Appends a comment that follows an element on the same line.
    fn trailing(&mut self, end: usize, out: &mut String) {
        let Some(c) = self.comments.get(self.next).copied() else { return };
        if c.start < end {
            return;
        }
        let between = &self.src[end..c.start];
        if between.chars().all(|ch| matches!(ch, ' ' | '\t' | ',')) {
            out.push_str("  ");
            out.push_str(self.text(c).trim_end());
            self.next += 1;
        }
    }

    // ---- program ----

    fn program(&mut self, program: &Program) -> String {
        let mut out = String::new();
        for (i, import) in program.imports.iter().enumerate() {
            self.before(import.span.start, 0, &mut out, i == 0);
            out.push_str(&format!("import {}", self.text(import.span).trim_start_matches("import").trim()));
            self.trailing(import.span.end, &mut out);
            out.push('\n');
        }
        for item in &program.items {
            let span = item.span();
            if !out.is_empty() {
                out.push('\n');
            }
            if self.leading(span.start, 0, &mut out, true) && self.blank_before(span.start) {
                out.push('\n');
            }
            let text = match item {
                Item::Fn(f) => self.fn_decl(f),
                Item::Struct(s) => self.struct_decl(s),
                Item::Enum(e) => self.enum_decl(e),
                Item::Test(t) => {
                    let name = self.text(t.name_span).to_string();
                    format!("test {name}{} {}", effects(&t.effects), self.block(&t.body, 0))
                }
            };
            out.push_str(&text);
            self.trailing(span.end, &mut out);
            out.push('\n');
        }
        if self.next < self.comments.len() {
            if !out.is_empty() {
                out.push('\n');
            }
            self.leading(usize::MAX, 0, &mut out, true);
        }
        out
    }

    fn fn_decl(&mut self, f: &FnDecl) -> String {
        let params: Vec<String> = f.params.iter().map(|p| format!("{}: {}", p.name, p.ty)).collect();
        let ret = match &f.ret {
            Some(t) if matches!(t.kind, TypeKind::Fn(..)) => format!(" -> ({t})"),
            Some(t) => format!(" -> {t}"),
            None => String::new(),
        };
        let head = format!("fn {}{}", f.name, generics(&f.type_params));
        let tail = format!("{ret}{}", effects(&f.effects));
        let mut out = format!("{head}({}){tail}", params.join(", "));
        if out.len() + 2 > WIDTH && !params.is_empty() {
            let lines: String = params.iter().map(|p| format!("  {p},\n")).collect();
            out = format!("{head}(\n{lines}){tail}");
        }
        for r in &f.requires {
            out.push_str(&format!("\n  requires {}", self.expr(r, 1, 9)));
        }
        for e in &f.ensures {
            out.push_str(&format!("\n  ensures {}", self.expr(e, 1, 8)));
        }
        out.push_str(if f.requires.is_empty() && f.ensures.is_empty() { " " } else { "\n" });
        out.push_str(&self.block(&f.body, 0));
        out
    }

    fn struct_decl(&mut self, s: &StructDecl) -> String {
        let head = format!("struct {}{} {{", s.name, generics(&s.type_params));
        if s.fields.is_empty() && !self.has_comment(s.span) {
            return format!("{head}}}");
        }
        let mut out = format!("{head}\n");
        for (i, field) in s.fields.iter().enumerate() {
            self.before(field.span.start, 1, &mut out, i == 0);
            out.push_str(&format!("  {}: {},", field.name, field.ty));
            self.trailing(field.ty.span.end, &mut out);
            out.push('\n');
        }
        self.leading(s.span.end - 1, 1, &mut out, s.fields.is_empty());
        out.push('}');
        out
    }

    fn enum_decl(&mut self, e: &EnumDecl) -> String {
        let head = format!("enum {}{} {{", e.name, generics(&e.type_params));
        if e.variants.is_empty() && !self.has_comment(e.span) {
            return format!("{head}}}");
        }
        let mut out = format!("{head}\n");
        for (i, v) in e.variants.iter().enumerate() {
            self.before(v.span.start, 1, &mut out, i == 0);
            out.push_str(&format!("  {}", v.name));
            if !v.fields.is_empty() {
                let tys: Vec<String> = v.fields.iter().map(|t| t.to_string()).collect();
                out.push_str(&format!("({})", tys.join(", ")));
            }
            out.push(',');
            self.trailing(v.span.end, &mut out);
            out.push('\n');
        }
        self.leading(e.span.end - 1, 1, &mut out, e.variants.is_empty());
        out.push('}');
        out
    }

    // ---- statements ----

    /// A block whose opening brace is on a line indented by `level`.
    fn block(&mut self, block: &Block, level: usize) -> String {
        if block.stmts.is_empty() && !self.has_comment(block.span) {
            return "{}".to_string();
        }
        let mut out = String::from("{\n");
        for (i, stmt) in block.stmts.iter().enumerate() {
            let span = stmt.span();
            self.before(span.start, level + 1, &mut out, i == 0);
            out.push_str(&ind(level + 1));
            out.push_str(&self.stmt(stmt, level + 1));
            self.trailing(span.end, &mut out);
            out.push('\n');
        }
        self.leading(block.span.end - 1, level + 1, &mut out, block.stmts.is_empty());
        out.push_str(&ind(level));
        out.push('}');
        out
    }

    fn stmt(&mut self, stmt: &Stmt, level: usize) -> String {
        match stmt {
            Stmt::Let { name, mutable, ty, value, .. } => {
                let mut head = String::from("let ");
                if *mutable {
                    head.push_str("mut ");
                }
                head.push_str(name);
                if let Some(t) = ty {
                    head.push_str(&format!(": {t}"));
                }
                head.push_str(" = ");
                let value = self.expr(value, level, head.len());
                format!("{head}{value}")
            }
            Stmt::Assign { target, value } => {
                let head = format!("{} = ", self.expr(target, level, 0));
                let value = self.expr(value, level, head.len());
                format!("{head}{value}")
            }
            Stmt::While { cond, body, .. } => {
                format!("while {} {}", self.expr(cond, level, 6), self.block(body, level))
            }
            Stmt::For { var, iter, end, body, .. } => {
                let mut head = format!("for {var} in {}", self.expr(iter, level, 8 + var.len()));
                if let Some(end) = end {
                    head.push_str(&format!("..{}", self.sub(end, 1, level)));
                }
                format!("{head} {}", self.block(body, level))
            }
            Stmt::Assert { cond, .. } => format!("assert {}", self.expr(cond, level, 7)),
            Stmt::Expr(e) => self.expr(e, level, 0),
        }
    }

    // ---- expressions: single-line form ----

    /// The expression on one line, or None if it needs several lines.
    fn inline(&self, e: &Expr) -> Option<String> {
        if self.has_comment(e.span) {
            return None;
        }
        Some(match &e.kind {
            ExprKind::Int(_) | ExprKind::Float(_) | ExprKind::Str(_) => self.text(e.span).to_string(),
            ExprKind::Bool(b) => b.to_string(),
            ExprKind::Unit => "()".to_string(),
            ExprKind::Ident(name) | ExprKind::FnRef(name) => name.clone(),
            ExprKind::Unary(op, operand) => format!("{}{}", unop(*op), self.inline_sub(operand, 6)?),
            ExprKind::Binary(op, lhs, rhs) => {
                let p = binprec(*op);
                let left = if op.is_comparison() { p + 1 } else { p };
                format!("{} {} {}", self.inline_sub(lhs, left)?, op.symbol(), self.inline_sub(rhs, p + 1)?)
            }
            ExprKind::Field(base, name, _) => format!("{}.{name}", self.inline_sub(base, 7)?),
            ExprKind::Index(base, index) => format!("{}[{}]", self.inline_sub(base, 7)?, self.inline(index)?),
            ExprKind::Try(inner) => format!("{}?", self.inline_sub(inner, 7)?),
            ExprKind::Return(None) => "return".to_string(),
            ExprKind::Return(Some(v)) => format!("return {}", self.inline(v)?),
            ExprKind::Break => "break".to_string(),
            ExprKind::Continue => "continue".to_string(),
            ExprKind::Ctor { enum_name, variant, args } if args.is_empty() => ctor_name(enum_name, variant),
            ExprKind::If { cond, then, els } => {
                let head = format!("if {} {}", self.inline(cond)?, self.inline_block(then)?);
                match els {
                    None => head,
                    Some(els) => match &els.kind {
                        ExprKind::Block(b) => format!("{head} else {}", self.inline_block(b)?),
                        _ => format!("{head} else {}", self.inline(els)?),
                    },
                }
            }
            ExprKind::Match { .. } => return None,
            ExprKind::Block(b) if b.stmts.is_empty() => "{}".to_string(),
            ExprKind::Block(_) => return None,
            ExprKind::Closure { params, body, .. } => {
                format!("{} {}", self.closure_head(params), self.inline(body)?)
            }
            _ => {
                let s = seq(e)?;
                let mut head = match s.recv {
                    Some(r) => self.inline_sub(r, 7)?,
                    None => String::new(),
                };
                head.push_str(&s.open);
                if s.items.is_empty() {
                    return Some(format!("{head}{}", s.close));
                }
                let mut parts = Vec::new();
                for (label, item) in &s.items {
                    let value = self.inline(item)?;
                    parts.push(match label {
                        Label::None => value,
                        Label::Field(name) => format!("{name}: {value}"),
                        Label::Key(key) => format!("{}: {value}", self.inline(key)?),
                    });
                }
                let pad = if s.pad { " " } else { "" };
                format!("{head}{pad}{}{pad}{}", parts.join(", "), s.close)
            }
        })
    }

    fn inline_sub(&self, e: &Expr, min: u8) -> Option<String> {
        let s = self.inline(e)?;
        Some(if prec(e) < min { format!("({s})") } else { s })
    }

    /// `{ expr }` for a block holding a single expression.
    fn inline_block(&self, block: &Block) -> Option<String> {
        if self.has_comment(block.span) {
            return None;
        }
        match block.stmts.as_slice() {
            [] => Some("{}".to_string()),
            [Stmt::Expr(e)] => Some(format!("{{ {} }}", self.inline(e)?)),
            _ => None,
        }
    }

    fn closure_head(&self, params: &[ClosureParam]) -> String {
        let params: Vec<String> = params
            .iter()
            .map(|p| match &p.ty {
                Some(t) => format!("{}: {t}", p.name),
                None => p.name.clone(),
            })
            .collect();
        format!("|{}|", params.join(", "))
    }

    // ---- expressions: general form ----

    /// Formats an expression that starts `used` columns into a line indented by `level`.
    fn expr(&mut self, e: &Expr, level: usize, used: usize) -> String {
        if let Some(s) = self.inline(e) {
            let breakable = match &e.kind {
                ExprKind::If { .. } | ExprKind::Closure { .. } => true,
                _ => seq(e).is_some_and(|s| !s.items.is_empty()),
            };
            if !breakable || level * 2 + used + s.len() <= WIDTH {
                return s;
            }
        }
        match &e.kind {
            ExprKind::Unary(op, operand) => format!("{}{}", unop(*op), self.sub(operand, 6, level)),
            ExprKind::Binary(op, lhs, rhs) => {
                let p = binprec(*op);
                let left = if op.is_comparison() { p + 1 } else { p };
                let lhs = self.sub(lhs, left, level);
                let rhs = self.sub(rhs, p + 1, level);
                format!("{lhs} {} {rhs}", op.symbol())
            }
            ExprKind::Field(base, name, _) => format!("{}.{name}", self.sub(base, 7, level)),
            ExprKind::Index(base, index) => {
                let base = self.sub(base, 7, level);
                format!("{base}[{}]", self.expr(index, level, 0))
            }
            ExprKind::Try(inner) => format!("{}?", self.sub(inner, 7, level)),
            ExprKind::Return(Some(v)) => format!("return {}", self.expr(v, level, used + 7)),
            ExprKind::If { cond, then, els } => self.if_multi(cond, then, els.as_deref(), level),
            ExprKind::Match { scrutinee, arms } => {
                let mut out = format!("match {} {{\n", self.expr(scrutinee, level, 6));
                for (i, arm) in arms.iter().enumerate() {
                    self.before(arm.pat.span.start, level + 1, &mut out, i == 0);
                    let head = format!("{} => ", self.pattern(&arm.pat));
                    let body = self.expr(&arm.body, level + 1, head.len());
                    out.push_str(&format!("{}{head}{body}", ind(level + 1)));
                    self.trailing(arm.body.span.end, &mut out);
                    out.push('\n');
                }
                self.leading(e.span.end - 1, level + 1, &mut out, arms.is_empty());
                out.push_str(&ind(level));
                out.push('}');
                out
            }
            ExprKind::Block(b) => self.block(b, level),
            ExprKind::Closure { params, body, .. } => {
                let head = self.closure_head(params);
                let body = self.expr(body, level, used + head.len() + 1);
                format!("{head} {body}")
            }
            _ => match seq(e) {
                Some(s) => self.seq_multi(s, level),
                // A leaf that could not be printed inline (a comment inside it).
                None => self.text(e.span).to_string(),
            },
        }
    }

    fn sub(&mut self, e: &Expr, min: u8, level: usize) -> String {
        let s = self.expr(e, level, 0);
        if prec(e) < min {
            format!("({s})")
        } else {
            s
        }
    }

    fn if_multi(&mut self, cond: &Expr, then: &Block, els: Option<&Expr>, level: usize) -> String {
        let mut out = format!("if {} ", self.expr(cond, level, 3));
        out.push_str(&self.block(then, level));
        if let Some(els) = els {
            out.push_str(" else ");
            let rest = match &els.kind {
                ExprKind::If { cond, then, els } => self.if_multi(cond, then, els.as_deref(), level),
                ExprKind::Block(b) => self.block(b, level),
                _ => self.expr(els, level, 0),
            };
            out.push_str(&rest);
        }
        out
    }

    fn seq_multi(&mut self, s: Seq<'_>, level: usize) -> String {
        let mut out = match s.recv {
            Some(r) => self.sub(r, 7, level),
            None => String::new(),
        };
        out.push_str(&s.open);
        let Some(((_, last), init)) = s.items.split_last() else {
            out.push_str(s.close);
            return out;
        };

        // `f(a, b, |x| { ... })`: keep the call on one line and let the last argument span lines.
        // A single argument is never moved to its own line: `Some(Point { ... })`.
        let hug = !s.pad
            && (init.is_empty()
                || (self.inline(last).is_none() && init.iter().all(|(_, item)| self.inline(item).is_some())));
        if hug {
            for (_, item) in init {
                out.push_str(&self.inline(item).unwrap_or_default());
                out.push_str(", ");
            }
            out.push_str(&self.expr(last, level, 0));
            out.push_str(s.close);
            return out;
        }

        out.push('\n');
        for (label, item) in &s.items {
            let label = match label {
                Label::None => String::new(),
                Label::Field(name) => format!("{name}: "),
                Label::Key(key) => format!("{}: ", self.expr(key, level + 1, 0)),
            };
            let value = self.expr(item, level + 1, label.len());
            out.push_str(&format!("{}{label}{value},\n", ind(level + 1)));
        }
        out.push_str(&ind(level));
        out.push_str(s.close);
        out
    }

    fn pattern(&self, pat: &Pattern) -> String {
        match &pat.kind {
            PatKind::Wildcard => "_".to_string(),
            PatKind::Bind(name) => name.clone(),
            PatKind::Int(_) | PatKind::Str(_) | PatKind::Bool(_) => self.text(pat.span).to_string(),
            PatKind::Ctor { enum_name, variant, args } => {
                let name = ctor_name(enum_name, variant);
                if args.is_empty() {
                    name
                } else {
                    let args: Vec<String> = args.iter().map(|a| self.pattern(a)).collect();
                    format!("{name}({})", args.join(", "))
                }
            }
        }
    }
}
