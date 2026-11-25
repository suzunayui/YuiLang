//! Recursive-descent parser. Stops at the first syntax error.
//!
//! The parser tracks local variables so that it can tell locals from top-level
//! names, record which variables a closure captures, and (when `resolve` is on)
//! rewrite references to top-level names into module-qualified names.

use crate::ast::*;
use crate::diag::{Diagnostic, Span};
use crate::lexer::{self, Tok, Token};
use std::collections::HashMap;
use std::rc::Rc;

type P<T> = Result<T, Diagnostic>;

/// Parses one file. `base` is the offset of the file in the source map.
/// `module` is the dotted module path (empty for the entry file). With
/// `resolve` off, names are kept exactly as written (used by the formatter).
pub fn parse_file(src: &str, base: usize, module: &str, resolve: bool) -> P<Program> {
    let toks = lexer::lex(src, base)?;
    let mut p = Parser {
        toks,
        pos: 0,
        module: module.to_string(),
        resolve,
        aliases: HashMap::new(),
        type_params: Vec::new(),
        scopes: Vec::new(),
        closures: Vec::new(),
    };
    let mut imports = Vec::new();
    while *p.peek() == Tok::Import {
        imports.push(p.import()?);
    }
    let mut items = Vec::new();
    while *p.peek() != Tok::Eof {
        items.push(p.item()?);
    }
    Ok(Program { imports, items, types: TypeInfo::default() })
}

struct Parser {
    toks: Vec<Token>,
    pos: usize,
    module: String,
    resolve: bool,
    /// Import alias -> canonical module path.
    aliases: HashMap<String, String>,
    type_params: Vec<String>,
    scopes: Vec<Vec<String>>,
    /// Closures being parsed: index of the parameter scope and captured names.
    closures: Vec<(usize, Vec<String>)>,
}

fn is_upper(s: &str) -> bool {
    s.chars().next().is_some_and(|c| c.is_ascii_uppercase())
}

impl Parser {
    fn peek(&self) -> &Tok {
        &self.toks[self.pos].tok
    }

    fn peek_at(&self, n: usize) -> &Tok {
        &self.toks[(self.pos + n).min(self.toks.len() - 1)].tok
    }

    fn span(&self) -> Span {
        self.toks[self.pos].span
    }

    fn prev_span(&self) -> Span {
        self.toks[self.pos.saturating_sub(1)].span
    }

    /// True when the current token is the first token on its line.
    fn nl(&self) -> bool {
        self.toks[self.pos].nl_before
    }

    fn bump(&mut self) -> Token {
        let t = self.toks[self.pos].clone();
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        t
    }

    fn eat(&mut self, t: &Tok) -> bool {
        if self.peek() == t {
            self.bump();
            true
        } else {
            false
        }
    }

    fn error(&self, msg: impl Into<String>) -> Diagnostic {
        Diagnostic::new("E0101", msg, self.span())
    }

    fn expect(&mut self, t: Tok, what: &str) -> P<Span> {
        if *self.peek() == t {
            Ok(self.bump().span)
        } else {
            Err(self.error(format!("expected {what}, found {}", self.peek().describe())))
        }
    }

    fn ident(&mut self, what: &str) -> P<(String, Span)> {
        match self.peek().clone() {
            Tok::Ident(name) => {
                let span = self.bump().span;
                Ok((name, span))
            }
            other => Err(self.error(format!("expected {what}, found {}", other.describe()))),
        }
    }

    fn lower_ident(&mut self, what: &str) -> P<(String, Span)> {
        let (name, span) = self.ident(what)?;
        if is_upper(&name) {
            return Err(Diagnostic::new(
                "E0104",
                format!("{what} `{name}` must start with a lowercase letter"),
                span,
            )
            .hint("names starting with an uppercase letter are reserved for types"));
        }
        Ok((name, span))
    }

    fn upper_ident(&mut self, what: &str) -> P<(String, Span)> {
        let (name, span) = self.ident(what)?;
        if !is_upper(&name) {
            return Err(Diagnostic::new(
                "E0104",
                format!("{what} `{name}` must start with an uppercase letter"),
                span,
            ));
        }
        Ok((name, span))
    }

    // ---- names ----

    fn qualify(&self, name: &str) -> String {
        if self.resolve && !self.module.is_empty() {
            format!("{}.{name}", self.module)
        } else {
            name.to_string()
        }
    }

    fn qualify_fn(&self, name: &str) -> String {
        if BUILTIN_FNS.contains(&name) {
            name.to_string()
        } else {
            self.qualify(name)
        }
    }

    fn qualify_type(&self, name: &str) -> String {
        if BUILTIN_TYPES.contains(&name) || self.type_params.iter().any(|t| t == name) {
            name.to_string()
        } else {
            self.qualify(name)
        }
    }

    fn declare(&mut self, name: &str) {
        if let Some(scope) = self.scopes.last_mut() {
            scope.push(name.to_string());
        }
    }

    /// Whether `name` is a local variable. Also records closure captures.
    fn is_local(&mut self, name: &str) -> bool {
        let Some(idx) = self.scopes.iter().rposition(|s| s.iter().any(|n| n == name)) else {
            return false;
        };
        for (base, captures) in &mut self.closures {
            if *base > idx && !captures.iter().any(|c| c == name) {
                captures.push(name.to_string());
            }
        }
        true
    }

    /// If the current tokens are `alias . Name`, consumes them and returns the
    /// qualified name and whether `Name` starts with an uppercase letter.
    fn module_member(&mut self, alias: &str) -> P<Option<(String, bool, Span)>> {
        if *self.peek() != Tok::Dot {
            return Ok(None);
        }
        let Some(path) = self.aliases.get(alias).cloned() else { return Ok(None) };
        self.bump();
        let (member, span) = self.ident("a name after the module")?;
        let upper = is_upper(&member);
        Ok(Some((format!("{path}.{member}"), upper, span)))
    }

    // ---- items ----

    fn import(&mut self) -> P<Import> {
        let start = self.bump().span;
        let mut segments = vec![self.lower_ident("module name")?.0];
        while self.eat(&Tok::Dot) {
            segments.push(self.lower_ident("module name")?.0);
        }
        let span = start.to(self.prev_span());
        let path = segments.join(".");
        let alias = segments.pop().unwrap_or_default();
        if self.aliases.contains_key(&alias) {
            return Err(Diagnostic::new("E0106", format!("module name `{alias}` is imported more than once"), span));
        }
        let target = if self.resolve { path.clone() } else { alias.clone() };
        self.aliases.insert(alias.clone(), target);
        Ok(Import { path, alias, span })
    }

    fn item(&mut self) -> P<Item> {
        self.type_params.clear();
        match self.peek() {
            Tok::Fn => self.fn_decl(),
            Tok::Struct => self.struct_decl(),
            Tok::Enum => self.enum_decl(),
            Tok::Test => self.test_decl(),
            Tok::Import => Err(self.error("`import` must come before all other declarations")),
            other => Err(self
                .error(format!("expected `fn`, `struct`, `enum` or `test`, found {}", other.describe()))
                .hint("only declarations are allowed at the top level; put statements inside `fn main()`")),
        }
    }

    fn type_param_list(&mut self) -> P<Vec<String>> {
        let mut params = Vec::new();
        if self.eat(&Tok::Lt) {
            loop {
                params.push(self.upper_ident("type parameter")?.0);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
            self.expect(Tok::Gt, "`>`")?;
        }
        Ok(params)
    }

    fn effects(&mut self) -> P<Vec<(String, Span)>> {
        let mut effects = Vec::new();
        if self.eat(&Tok::Uses) {
            loop {
                effects.push(self.lower_ident("effect name")?);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        Ok(effects)
    }

    fn fn_decl(&mut self) -> P<Item> {
        let start = self.bump().span;
        let (name, name_span) = self.lower_ident("function name")?;
        self.type_params = self.type_param_list()?;
        self.expect(Tok::LParen, "`(`")?;
        let params = self.params(Tok::RParen)?;
        let ret = if self.eat(&Tok::Arrow) { Some(self.type_expr()?) } else { None };
        let effects = self.effects()?;

        self.scopes = vec![params.iter().map(|p| p.name.clone()).collect()];
        let mut requires = Vec::new();
        let mut ensures = Vec::new();
        loop {
            if self.eat(&Tok::Requires) {
                requires.push(self.expr()?);
            } else if self.eat(&Tok::Ensures) {
                self.scopes.push(vec!["result".to_string()]);
                let e = self.expr();
                self.scopes.pop();
                ensures.push(e?);
            } else {
                break;
            }
        }
        let body = self.block()?;
        self.scopes.clear();
        let span = start.to(body.span);
        Ok(Item::Fn(Rc::new(FnDecl {
            name: self.qualify(&name),
            name_span,
            span,
            type_params: std::mem::take(&mut self.type_params),
            params,
            ret,
            effects,
            requires,
            ensures,
            body,
        })))
    }

    /// Parses `name: Type, ...` up to and including the closing token.
    fn params(&mut self, close: Tok) -> P<Vec<Param>> {
        let mut params = Vec::new();
        while *self.peek() != close {
            let (name, span) = self.lower_ident("name")?;
            self.expect(Tok::Colon, "`:`")?;
            let ty = self.type_expr()?;
            params.push(Param { name, ty, span });
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(close.clone(), &close.describe())?;
        Ok(params)
    }

    fn struct_decl(&mut self) -> P<Item> {
        let start = self.bump().span;
        let (name, name_span) = self.upper_ident("struct name")?;
        self.type_params = self.type_param_list()?;
        self.expect(Tok::LBrace, "`{`")?;
        let fields = self.params(Tok::RBrace)?;
        let span = start.to(self.prev_span());
        Ok(Item::Struct(StructDecl {
            name: self.qualify(&name),
            name_span,
            span,
            type_params: std::mem::take(&mut self.type_params),
            fields,
        }))
    }

    fn enum_decl(&mut self) -> P<Item> {
        let start = self.bump().span;
        let (name, name_span) = self.upper_ident("enum name")?;
        self.type_params = self.type_param_list()?;
        self.expect(Tok::LBrace, "`{`")?;
        let mut variants = Vec::new();
        while *self.peek() != Tok::RBrace {
            let (vname, vspan) = self.upper_ident("variant name")?;
            let mut fields = Vec::new();
            if self.eat(&Tok::LParen) {
                while *self.peek() != Tok::RParen {
                    fields.push(self.type_expr()?);
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                self.expect(Tok::RParen, "`)`")?;
            }
            variants.push(Variant { name: vname, span: vspan.to(self.prev_span()), fields });
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(Tok::RBrace, "`}`")?;
        let span = start.to(self.prev_span());
        Ok(Item::Enum(EnumDecl {
            name: self.qualify(&name),
            name_span,
            span,
            type_params: std::mem::take(&mut self.type_params),
            variants,
        }))
    }

    fn test_decl(&mut self) -> P<Item> {
        let start = self.bump().span;
        let name_span = self.span();
        let name = match self.peek().clone() {
            Tok::Str(parts) => match parts.as_slice() {
                [lexer::StrPart::Lit(s)] => {
                    self.bump();
                    s.clone()
                }
                _ => return Err(self.error("test names cannot use `{...}` interpolation")),
            },
            other => return Err(self.error(format!("expected test name string, found {}", other.describe()))),
        };
        let effects = self.effects()?;
        self.scopes = vec![Vec::new()];
        let body = self.block()?;
        self.scopes.clear();
        let span = start.to(body.span);
        Ok(Item::Test(Rc::new(TestDecl { name, name_span, span, effects, body, root: self.module.is_empty() })))
    }

    fn type_expr(&mut self) -> P<TypeExpr> {
        let start = self.span();
        match self.peek().clone() {
            Tok::Fn => {
                self.bump();
                self.expect(Tok::LParen, "`(`")?;
                let mut params = Vec::new();
                while *self.peek() != Tok::RParen {
                    params.push(self.type_expr()?);
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                self.expect(Tok::RParen, "`)`")?;
                let ret = if self.eat(&Tok::Arrow) { Some(Box::new(self.type_expr()?)) } else { None };
                let mut effects = Vec::new();
                if self.eat(&Tok::Uses) {
                    loop {
                        effects.push(self.lower_ident("effect name")?.0);
                        // A comma continues the effect list only when an effect name follows;
                        // otherwise it separates the next parameter.
                        let more = *self.peek() == Tok::Comma
                            && matches!(self.peek_at(1), Tok::Ident(n) if EFFECTS.contains(&n.as_str()))
                            && *self.peek_at(2) != Tok::Colon;
                        if !more {
                            break;
                        }
                        self.bump();
                    }
                }
                Ok(TypeExpr { kind: TypeKind::Fn(params, ret, effects), span: start.to(self.prev_span()) })
            }
            Tok::LParen => {
                self.bump();
                let inner = self.type_expr()?;
                self.expect(Tok::RParen, "`)`")?;
                Ok(inner)
            }
            Tok::Ident(alias) if !is_upper(&alias) => {
                self.bump();
                match self.module_member(&alias)? {
                    Some((full, true, _)) => self.type_args(full, start),
                    _ => Err(Diagnostic::new("E0101", format!("expected a type, found `{alias}`"), start)
                        .hint("type names start with an uppercase letter")),
                }
            }
            _ => {
                let (name, _) = self.upper_ident("type name")?;
                let full = self.qualify_type(&name);
                self.type_args(full, start)
            }
        }
    }

    fn type_args(&mut self, name: String, start: Span) -> P<TypeExpr> {
        let mut args = Vec::new();
        if self.eat(&Tok::Lt) {
            loop {
                args.push(self.type_expr()?);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
            self.expect(Tok::Gt, "`>`")?;
        }
        Ok(TypeExpr { kind: TypeKind::Named(name, args), span: start.to(self.prev_span()) })
    }

    // ---- statements ----

    fn block(&mut self) -> P<Block> {
        self.scopes.push(Vec::new());
        let result = self.block_inner();
        self.scopes.pop();
        result
    }

    fn block_inner(&mut self) -> P<Block> {
        let start = self.expect(Tok::LBrace, "`{`")?;
        let mut stmts = Vec::new();
        while *self.peek() != Tok::RBrace {
            if *self.peek() == Tok::Eof {
                return Err(self.error("expected `}`, found end of file"));
            }
            stmts.push(self.stmt()?);
            if *self.peek() != Tok::RBrace && !self.nl() {
                return Err(Diagnostic::new(
                    "E0103",
                    format!("expected a new line before {}", self.peek().describe()),
                    self.span(),
                )
                .hint("write one statement per line"));
            }
        }
        let end = self.bump().span;
        Ok(Block { stmts, span: start.to(end) })
    }

    fn stmt(&mut self) -> P<Stmt> {
        let start = self.span();
        match self.peek() {
            Tok::Let => {
                self.bump();
                let mutable = self.eat(&Tok::Mut);
                let (name, name_span) = self.lower_ident("variable name")?;
                let ty = if self.eat(&Tok::Colon) { Some(self.type_expr()?) } else { None };
                self.expect(Tok::Eq, "`=`")?;
                let value = self.expr()?;
                self.declare(&name);
                let span = start.to(value.span);
                Ok(Stmt::Let { name, name_span, mutable, ty, value, span })
            }
            Tok::While => {
                self.bump();
                let cond = self.expr()?;
                let body = self.block()?;
                let span = start.to(body.span);
                Ok(Stmt::While { cond, body, span })
            }
            Tok::For => {
                self.bump();
                let (var, var_span) = self.lower_ident("loop variable")?;
                self.expect(Tok::In, "`in`")?;
                let iter = self.expr()?;
                let end = if self.eat(&Tok::DotDot) { Some(self.expr()?) } else { None };
                self.scopes.push(vec![var.clone()]);
                let body = self.block();
                self.scopes.pop();
                let body = body?;
                let span = start.to(body.span);
                Ok(Stmt::For { var, var_span, iter, end, body, span })
            }
            Tok::Assert => {
                self.bump();
                let cond = self.expr()?;
                let span = start.to(cond.span);
                Ok(Stmt::Assert { cond, span })
            }
            _ => {
                let e = self.expr()?;
                if *self.peek() == Tok::Eq {
                    if !is_place(&e) {
                        return Err(Diagnostic::new("E0102", "invalid assignment target", e.span)
                            .hint("only variables, fields (`a.b`) and elements (`a[i]`) can be assigned"));
                    }
                    self.bump();
                    let value = self.expr()?;
                    Ok(Stmt::Assign { target: e, value })
                } else {
                    Ok(Stmt::Expr(e))
                }
            }
        }
    }

    // ---- expressions ----

    fn expr(&mut self) -> P<Expr> {
        self.or_expr()
    }

    /// Returns the binary operator at the current position, unless it starts a new line.
    fn binop(&self, table: &[(Tok, BinOp)]) -> Option<BinOp> {
        if self.nl() {
            return None;
        }
        table.iter().find(|(t, _)| t == self.peek()).map(|(_, op)| *op)
    }

    fn or_expr(&mut self) -> P<Expr> {
        let mut lhs = self.and_expr()?;
        while let Some(op) = self.binop(&[(Tok::OrOr, BinOp::Or)]) {
            self.bump();
            let rhs = self.and_expr()?;
            lhs = binary(op, lhs, rhs);
        }
        Ok(lhs)
    }

    fn and_expr(&mut self) -> P<Expr> {
        let mut lhs = self.cmp_expr()?;
        while let Some(op) = self.binop(&[(Tok::AndAnd, BinOp::And)]) {
            self.bump();
            let rhs = self.cmp_expr()?;
            lhs = binary(op, lhs, rhs);
        }
        Ok(lhs)
    }

    fn cmp_expr(&mut self) -> P<Expr> {
        const TABLE: &[(Tok, BinOp)] = &[
            (Tok::EqEq, BinOp::Eq),
            (Tok::NotEq, BinOp::Ne),
            (Tok::Lt, BinOp::Lt),
            (Tok::Le, BinOp::Le),
            (Tok::Gt, BinOp::Gt),
            (Tok::Ge, BinOp::Ge),
        ];
        let lhs = self.add_expr()?;
        if let Some(op) = self.binop(TABLE) {
            self.bump();
            let rhs = self.add_expr()?;
            if self.binop(TABLE).is_some() {
                return Err(Diagnostic::new("E0105", "comparison operators cannot be chained", self.span())
                    .hint("combine comparisons with `&&`, e.g. `a < b && b < c`"));
            }
            return Ok(binary(op, lhs, rhs));
        }
        Ok(lhs)
    }

    fn add_expr(&mut self) -> P<Expr> {
        let mut lhs = self.mul_expr()?;
        while let Some(op) = self.binop(&[(Tok::Plus, BinOp::Add), (Tok::Minus, BinOp::Sub)]) {
            self.bump();
            let rhs = self.mul_expr()?;
            lhs = binary(op, lhs, rhs);
        }
        Ok(lhs)
    }

    fn mul_expr(&mut self) -> P<Expr> {
        let mut lhs = self.unary_expr()?;
        while let Some(op) =
            self.binop(&[(Tok::Star, BinOp::Mul), (Tok::Slash, BinOp::Div), (Tok::Percent, BinOp::Rem)])
        {
            self.bump();
            let rhs = self.unary_expr()?;
            lhs = binary(op, lhs, rhs);
        }
        Ok(lhs)
    }

    fn unary_expr(&mut self) -> P<Expr> {
        let op = match self.peek() {
            Tok::Minus => UnOp::Neg,
            Tok::Bang => UnOp::Not,
            _ => return self.postfix_expr(),
        };
        let start = self.bump().span;
        let operand = self.unary_expr()?;
        let span = start.to(operand.span);
        Ok(Expr { kind: ExprKind::Unary(op, Box::new(operand)), span })
    }

    fn postfix_expr(&mut self) -> P<Expr> {
        let mut e = self.primary()?;
        loop {
            match self.peek() {
                // `.` may continue an expression on the next line.
                Tok::Dot => {
                    self.bump();
                    let (name, name_span) = self.lower_ident("field or method name")?;
                    if *self.peek() == Tok::LParen && !self.nl() {
                        self.bump();
                        let args = self.args(Tok::RParen)?;
                        let span = e.span.to(self.prev_span());
                        e = Expr {
                            kind: ExprKind::MethodCall { recv: Box::new(e), name, name_span, args },
                            span,
                        };
                    } else {
                        let span = e.span.to(name_span);
                        e = Expr { kind: ExprKind::Field(Box::new(e), name, name_span), span };
                    }
                }
                Tok::LBracket if !self.nl() => {
                    self.bump();
                    let index = self.expr()?;
                    let end = self.expect(Tok::RBracket, "`]`")?;
                    let span = e.span.to(end);
                    e = Expr { kind: ExprKind::Index(Box::new(e), Box::new(index)), span };
                }
                Tok::Question if !self.nl() => {
                    let end = self.bump().span;
                    let span = e.span.to(end);
                    e = Expr { kind: ExprKind::Try(Box::new(e)), span };
                }
                _ => return Ok(e),
            }
        }
    }

    /// Parses comma-separated expressions up to and including the closing token.
    fn args(&mut self, close: Tok) -> P<Vec<Expr>> {
        let mut args = Vec::new();
        while *self.peek() != close {
            args.push(self.expr()?);
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(close.clone(), &close.describe())?;
        Ok(args)
    }

    /// Parses the code of a `{...}` interpolation with the current scopes.
    fn interpolation(&mut self, code: &str, offset: usize) -> P<Expr> {
        let toks = lexer::lex(code, offset)?;
        let saved_toks = std::mem::replace(&mut self.toks, toks);
        let saved_pos = std::mem::replace(&mut self.pos, 0);
        let result = self.expr().and_then(|e| {
            if *self.peek() == Tok::Eof {
                Ok(e)
            } else {
                Err(self.error(format!(
                    "expected `}}` to end the interpolation, found {}",
                    self.peek().describe()
                )))
            }
        });
        self.toks = saved_toks;
        self.pos = saved_pos;
        result
    }

    /// A call or reference to a top-level function, after its name.
    fn global_ref(&mut self, name: String, start: Span) -> P<Expr> {
        if *self.peek() == Tok::LParen && !self.nl() {
            self.bump();
            let args = self.args(Tok::RParen)?;
            let span = start.to(self.prev_span());
            Ok(Expr { kind: ExprKind::Call { name, name_span: start, local: false, args }, span })
        } else {
            Ok(Expr { kind: ExprKind::FnRef(name), span: start.to(self.prev_span()) })
        }
    }

    fn primary(&mut self) -> P<Expr> {
        let start = self.span();
        match self.peek().clone() {
            Tok::Int(n) => {
                self.bump();
                Ok(Expr { kind: ExprKind::Int(n), span: start })
            }
            Tok::Float(n) => {
                self.bump();
                Ok(Expr { kind: ExprKind::Float(n), span: start })
            }
            Tok::True => {
                self.bump();
                Ok(Expr { kind: ExprKind::Bool(true), span: start })
            }
            Tok::False => {
                self.bump();
                Ok(Expr { kind: ExprKind::Bool(false), span: start })
            }
            Tok::Str(parts) => {
                self.bump();
                let mut out = Vec::new();
                for part in parts {
                    match part {
                        lexer::StrPart::Lit(s) => out.push(StrPart::Lit(s)),
                        lexer::StrPart::Code(code, offset) => {
                            out.push(StrPart::Expr(self.interpolation(&code, offset)?));
                        }
                    }
                }
                Ok(Expr { kind: ExprKind::Str(out), span: start })
            }
            Tok::Ident(name) => {
                self.bump();
                if is_upper(&name) {
                    return self.upper_primary(name, start);
                }
                if self.is_local(&name) {
                    if *self.peek() == Tok::LParen && !self.nl() {
                        self.bump();
                        let args = self.args(Tok::RParen)?;
                        let span = start.to(self.prev_span());
                        return Ok(Expr {
                            kind: ExprKind::Call { name, name_span: start, local: true, args },
                            span,
                        });
                    }
                    return Ok(Expr { kind: ExprKind::Ident(name), span: start });
                }
                match self.module_member(&name)? {
                    Some((full, true, _)) => self.upper_rest(full, start),
                    Some((full, false, _)) => self.global_ref(full, start),
                    None => {
                        let full = self.qualify_fn(&name);
                        self.global_ref(full, start)
                    }
                }
            }
            Tok::LParen => {
                self.bump();
                if *self.peek() == Tok::RParen {
                    let end = self.bump().span;
                    return Ok(Expr { kind: ExprKind::Unit, span: start.to(end) });
                }
                let e = self.expr()?;
                self.expect(Tok::RParen, "`)`")?;
                Ok(e)
            }
            Tok::LBracket => {
                self.bump();
                let items = self.args(Tok::RBracket)?;
                let span = start.to(self.prev_span());
                Ok(Expr { kind: ExprKind::List(items), span })
            }
            Tok::Pipe | Tok::OrOr => self.closure(),
            Tok::If => self.if_expr(),
            Tok::Match => self.match_expr(),
            Tok::LBrace => {
                let b = self.block()?;
                let span = b.span;
                Ok(Expr { kind: ExprKind::Block(b), span })
            }
            Tok::Return => {
                self.bump();
                let has_value =
                    !self.nl() && !matches!(self.peek(), Tok::RBrace | Tok::RParen | Tok::Comma | Tok::Eof);
                if has_value {
                    let v = self.expr()?;
                    let span = start.to(v.span);
                    Ok(Expr { kind: ExprKind::Return(Some(Box::new(v))), span })
                } else {
                    Ok(Expr { kind: ExprKind::Return(None), span: start })
                }
            }
            Tok::Break => {
                self.bump();
                Ok(Expr { kind: ExprKind::Break, span: start })
            }
            Tok::Continue => {
                self.bump();
                Ok(Expr { kind: ExprKind::Continue, span: start })
            }
            other => Err(self.error(format!("expected an expression, found {}", other.describe()))),
        }
    }

    fn closure(&mut self) -> P<Expr> {
        let start = self.span();
        let mut params = Vec::new();
        if !self.eat(&Tok::OrOr) {
            self.expect(Tok::Pipe, "`|`")?;
            while *self.peek() != Tok::Pipe {
                let (name, span) = self.lower_ident("parameter name")?;
                let ty = if self.eat(&Tok::Colon) { Some(self.type_expr()?) } else { None };
                params.push(ClosureParam { name, span, ty });
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
            self.expect(Tok::Pipe, "`|`")?;
        }
        self.scopes.push(params.iter().map(|p| p.name.clone()).collect());
        self.closures.push((self.scopes.len() - 1, Vec::new()));
        let body = self.expr();
        let captures = self.closures.pop().map(|(_, c)| c).unwrap_or_default();
        self.scopes.pop();
        let body = body?;
        let span = start.to(body.span);
        Ok(Expr { kind: ExprKind::Closure { params, captures, body: Rc::new(body) }, span })
    }

    /// An expression starting with an uppercase identifier (already consumed).
    fn upper_primary(&mut self, name: String, start: Span) -> P<Expr> {
        match name.as_str() {
            "Some" | "Ok" | "Err" => {
                self.expect(Tok::LParen, "`(`")?;
                let value = self.expr()?;
                self.eat(&Tok::Comma);
                let end = self.expect(Tok::RParen, "`)`")?;
                let enum_name = if name == "Some" { "Option" } else { "Result" };
                return Ok(Expr {
                    kind: ExprKind::Ctor { enum_name: enum_name.to_string(), variant: name, args: vec![value] },
                    span: start.to(end),
                });
            }
            "None" => {
                return Ok(Expr {
                    kind: ExprKind::Ctor { enum_name: "Option".to_string(), variant: name, args: vec![] },
                    span: start,
                });
            }
            "Map" if *self.peek() == Tok::LBrace => {
                self.bump();
                let mut entries = Vec::new();
                while *self.peek() != Tok::RBrace {
                    let key = self.expr()?;
                    self.expect(Tok::Colon, "`:`")?;
                    let value = self.expr()?;
                    entries.push((key, value));
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                let end = self.expect(Tok::RBrace, "`}`")?;
                return Ok(Expr { kind: ExprKind::MapLit(entries), span: start.to(end) });
            }
            _ => {}
        }
        let full = self.qualify_type(&name);
        self.upper_rest(full, start)
    }

    /// A struct literal or enum constructor, after the (qualified) type name.
    fn upper_rest(&mut self, name: String, start: Span) -> P<Expr> {
        match self.peek() {
            Tok::LBrace => {
                self.bump();
                let mut fields = Vec::new();
                while *self.peek() != Tok::RBrace {
                    let (fname, fspan) = self.lower_ident("field name")?;
                    self.expect(Tok::Colon, "`:`")?;
                    let value = self.expr()?;
                    fields.push((fname, fspan, value));
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                let end = self.expect(Tok::RBrace, "`}`")?;
                Ok(Expr { kind: ExprKind::StructLit { name, fields }, span: start.to(end) })
            }
            Tok::Dot => {
                self.bump();
                let (variant, vspan) = self.upper_ident("variant name")?;
                let mut args = Vec::new();
                let mut end = vspan;
                if *self.peek() == Tok::LParen && !self.nl() {
                    self.bump();
                    args = self.args(Tok::RParen)?;
                    end = self.prev_span();
                }
                Ok(Expr { kind: ExprKind::Ctor { enum_name: name, variant, args }, span: start.to(end) })
            }
            _ => Err(Diagnostic::new("E0101", format!("`{name}` is a type name, not a value"), start)
                .hint("write `Name { field: value }` for a struct or `Name.Variant` for an enum")),
        }
    }

    fn if_expr(&mut self) -> P<Expr> {
        let start = self.bump().span;
        let cond = self.expr()?;
        let then = self.block()?;
        let mut end = then.span;
        let els = if self.eat(&Tok::Else) {
            let e = if *self.peek() == Tok::If {
                self.if_expr()?
            } else {
                let b = self.block()?;
                let span = b.span;
                Expr { kind: ExprKind::Block(b), span }
            };
            end = e.span;
            Some(Box::new(e))
        } else {
            None
        };
        Ok(Expr { kind: ExprKind::If { cond: Box::new(cond), then, els }, span: start.to(end) })
    }

    fn match_expr(&mut self) -> P<Expr> {
        let start = self.bump().span;
        let scrutinee = self.expr()?;
        self.expect(Tok::LBrace, "`{`")?;
        let mut arms = Vec::new();
        while *self.peek() != Tok::RBrace {
            self.scopes.push(Vec::new());
            let arm = self.arm();
            self.scopes.pop();
            arms.push(arm?);
            let comma = self.eat(&Tok::Comma);
            if !comma && *self.peek() != Tok::RBrace && !self.nl() {
                return Err(self
                    .error(format!("expected `,` or a new line after a match arm, found {}", self.peek().describe())));
            }
        }
        let end = self.expect(Tok::RBrace, "`}`")?;
        Ok(Expr { kind: ExprKind::Match { scrutinee: Box::new(scrutinee), arms }, span: start.to(end) })
    }

    fn arm(&mut self) -> P<Arm> {
        let pat = self.pattern()?;
        self.expect(Tok::FatArrow, "`=>`")?;
        let body = self.expr()?;
        Ok(Arm { pat, body })
    }

    fn pattern(&mut self) -> P<Pattern> {
        let start = self.span();
        match self.peek().clone() {
            Tok::Int(n) => {
                self.bump();
                Ok(Pattern { kind: PatKind::Int(n), span: start })
            }
            Tok::Minus => {
                self.bump();
                match self.peek().clone() {
                    Tok::Int(n) => {
                        let end = self.bump().span;
                        Ok(Pattern { kind: PatKind::Int(-n), span: start.to(end) })
                    }
                    other => Err(self.error(format!("expected an integer after `-`, found {}", other.describe()))),
                }
            }
            Tok::True => {
                self.bump();
                Ok(Pattern { kind: PatKind::Bool(true), span: start })
            }
            Tok::False => {
                self.bump();
                Ok(Pattern { kind: PatKind::Bool(false), span: start })
            }
            Tok::Str(parts) => match parts.as_slice() {
                [lexer::StrPart::Lit(s)] => {
                    self.bump();
                    Ok(Pattern { kind: PatKind::Str(s.clone()), span: start })
                }
                _ => Err(self.error("string patterns cannot use `{...}` interpolation")),
            },
            Tok::Ident(name) => {
                self.bump();
                if name == "_" {
                    return Ok(Pattern { kind: PatKind::Wildcard, span: start });
                }
                let enum_name = if is_upper(&name) {
                    match name.as_str() {
                        "Some" | "None" => return self.ctor_pattern("Option".to_string(), name, start),
                        "Ok" | "Err" => return self.ctor_pattern("Result".to_string(), name, start),
                        _ => self.qualify_type(&name),
                    }
                } else {
                    match self.module_member(&name)? {
                        Some((full, true, _)) => full,
                        Some((full, false, span)) => {
                            return Err(Diagnostic::new("E0101", format!("`{full}` is not a pattern"), span));
                        }
                        None => {
                            self.declare(&name);
                            return Ok(Pattern { kind: PatKind::Bind(name), span: start });
                        }
                    }
                };
                self.expect(Tok::Dot, "`.` (enum patterns are written `Enum.Variant`)")?;
                let (variant, _) = self.upper_ident("variant name")?;
                self.ctor_pattern(enum_name, variant, start)
            }
            other => Err(self.error(format!("expected a pattern, found {}", other.describe()))),
        }
    }

    fn ctor_pattern(&mut self, enum_name: String, variant: String, start: Span) -> P<Pattern> {
        let mut args = Vec::new();
        if *self.peek() == Tok::LParen {
            self.bump();
            while *self.peek() != Tok::RParen {
                args.push(self.pattern()?);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
            self.expect(Tok::RParen, "`)`")?;
        }
        let span = start.to(self.prev_span());
        Ok(Pattern { kind: PatKind::Ctor { enum_name, variant, args }, span })
    }
}

fn binary(op: BinOp, lhs: Expr, rhs: Expr) -> Expr {
    let span = lhs.span.to(rhs.span);
    Expr { kind: ExprKind::Binary(op, Box::new(lhs), Box::new(rhs)), span }
}

fn is_place(e: &Expr) -> bool {
    match &e.kind {
        // A non-local name parses as `FnRef`; the checker reports it as undefined.
        ExprKind::Ident(_) | ExprKind::FnRef(_) => true,
        ExprKind::Field(base, _, _) => is_place(base),
        ExprKind::Index(base, _) => is_place(base),
        _ => false,
    }
}
