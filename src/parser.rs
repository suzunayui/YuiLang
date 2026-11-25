//! Recursive-descent parser. Stops at the first syntax error.

use crate::ast::*;
use crate::diag::{Diagnostic, Span};
use crate::lexer::{self, Tok, Token};
use std::rc::Rc;

type P<T> = Result<T, Diagnostic>;

pub fn parse_program(src: &str) -> P<Program> {
    let toks = lexer::lex(src, 0)?;
    let mut p = Parser { toks, pos: 0 };
    let mut items = Vec::new();
    while *p.peek() != Tok::Eof {
        items.push(p.item()?);
    }
    Ok(Program { items })
}

struct Parser {
    toks: Vec<Token>,
    pos: usize,
}

fn is_upper(s: &str) -> bool {
    s.chars().next().is_some_and(|c| c.is_ascii_uppercase())
}

impl Parser {
    fn peek(&self) -> &Tok {
        &self.toks[self.pos].tok
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

    // ---- items ----

    fn item(&mut self) -> P<Item> {
        match self.peek() {
            Tok::Fn => self.fn_decl(),
            Tok::Struct => self.struct_decl(),
            Tok::Enum => self.enum_decl(),
            Tok::Test => self.test_decl(),
            other => Err(self
                .error(format!("expected `fn`, `struct`, `enum` or `test`, found {}", other.describe()))
                .hint("only declarations are allowed at the top level; put statements inside `fn main()`")),
        }
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
        self.bump();
        let (name, name_span) = self.lower_ident("function name")?;
        self.expect(Tok::LParen, "`(`")?;
        let params = self.params(Tok::RParen)?;
        let ret = if self.eat(&Tok::Arrow) { Some(self.type_expr()?) } else { None };
        let effects = self.effects()?;
        let mut requires = Vec::new();
        let mut ensures = Vec::new();
        loop {
            if self.eat(&Tok::Requires) {
                requires.push(self.expr()?);
            } else if self.eat(&Tok::Ensures) {
                ensures.push(self.expr()?);
            } else {
                break;
            }
        }
        let body = self.block()?;
        Ok(Item::Fn(Rc::new(FnDecl { name, name_span, params, ret, effects, requires, ensures, body })))
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
        self.bump();
        let (name, name_span) = self.upper_ident("struct name")?;
        self.expect(Tok::LBrace, "`{`")?;
        let fields = self.params(Tok::RBrace)?;
        Ok(Item::Struct(StructDecl { name, name_span, fields }))
    }

    fn enum_decl(&mut self) -> P<Item> {
        self.bump();
        let (name, name_span) = self.upper_ident("enum name")?;
        self.expect(Tok::LBrace, "`{`")?;
        let mut variants = Vec::new();
        while *self.peek() != Tok::RBrace {
            let (vname, span) = self.upper_ident("variant name")?;
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
            variants.push(Variant { name: vname, span, fields });
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(Tok::RBrace, "`}`")?;
        Ok(Item::Enum(EnumDecl { name, name_span, variants }))
    }

    fn test_decl(&mut self) -> P<Item> {
        self.bump();
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
        let body = self.block()?;
        Ok(Item::Test(Rc::new(TestDecl { name, name_span, effects, body })))
    }

    fn type_expr(&mut self) -> P<TypeExpr> {
        let (name, span) = self.upper_ident("type name")?;
        let mut args = Vec::new();
        let mut end = span;
        if self.eat(&Tok::Lt) {
            loop {
                args.push(self.type_expr()?);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
            end = self.expect(Tok::Gt, "`>`")?;
        }
        Ok(TypeExpr { name, args, span: span.to(end) })
    }

    // ---- statements ----

    fn block(&mut self) -> P<Block> {
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
        match self.peek() {
            Tok::Let => {
                self.bump();
                let mutable = self.eat(&Tok::Mut);
                let (name, name_span) = self.lower_ident("variable name")?;
                let ty = if self.eat(&Tok::Colon) { Some(self.type_expr()?) } else { None };
                self.expect(Tok::Eq, "`=`")?;
                let value = self.expr()?;
                Ok(Stmt::Let { name, name_span, mutable, ty, value })
            }
            Tok::While => {
                self.bump();
                let cond = self.expr()?;
                let body = self.block()?;
                Ok(Stmt::While { cond, body })
            }
            Tok::For => {
                self.bump();
                let (var, var_span) = self.lower_ident("loop variable")?;
                self.expect(Tok::In, "`in`")?;
                let iter = self.expr()?;
                let end = if self.eat(&Tok::DotDot) { Some(self.expr()?) } else { None };
                let body = self.block()?;
                Ok(Stmt::For { var, var_span, iter, end, body })
            }
            Tok::Assert => {
                let start = self.bump().span;
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
                            let toks = lexer::lex(&code, offset)?;
                            let mut sub = Parser { toks, pos: 0 };
                            let e = sub.expr()?;
                            if *sub.peek() != Tok::Eof {
                                return Err(sub.error(format!(
                                    "expected `}}` to end the interpolation, found {}",
                                    sub.peek().describe()
                                )));
                            }
                            out.push(StrPart::Expr(e));
                        }
                    }
                }
                Ok(Expr { kind: ExprKind::Str(out), span: start })
            }
            Tok::Ident(name) => {
                self.bump();
                if is_upper(&name) {
                    self.upper_primary(name, start)
                } else if *self.peek() == Tok::LParen && !self.nl() {
                    self.bump();
                    let args = self.args(Tok::RParen)?;
                    let span = start.to(self.prev_span());
                    Ok(Expr { kind: ExprKind::Call { name, name_span: start, args }, span })
                } else {
                    Ok(Expr { kind: ExprKind::Ident(name), span: start })
                }
            }
            Tok::LParen => {
                self.bump();
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
            Tok::If => self.if_expr(),
            Tok::Match => self.match_expr(),
            Tok::LBrace => {
                let b = self.block()?;
                let span = b.span;
                Ok(Expr { kind: ExprKind::Block(b), span })
            }
            Tok::Return => {
                self.bump();
                let has_value = !self.nl() && !matches!(self.peek(), Tok::RBrace | Tok::Comma | Tok::Eof);
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

    /// An expression starting with an uppercase identifier (already consumed).
    fn upper_primary(&mut self, name: String, start: Span) -> P<Expr> {
        match name.as_str() {
            "Some" | "Ok" | "Err" => {
                self.expect(Tok::LParen, "`(`")?;
                let value = self.expr()?;
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
            _ => {}
        }
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
            let pat = self.pattern()?;
            self.expect(Tok::FatArrow, "`=>`")?;
            let body = self.expr()?;
            arms.push(Arm { pat, body });
            let comma = self.eat(&Tok::Comma);
            if !comma && *self.peek() != Tok::RBrace && !self.nl() {
                return Err(self
                    .error(format!("expected `,` or a new line after a match arm, found {}", self.peek().describe())));
            }
        }
        let end = self.expect(Tok::RBrace, "`}`")?;
        Ok(Expr { kind: ExprKind::Match { scrutinee: Box::new(scrutinee), arms }, span: start.to(end) })
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
                if !is_upper(&name) {
                    return Ok(Pattern { kind: PatKind::Bind(name), span: start });
                }
                let (enum_name, variant) = match name.as_str() {
                    "Some" | "None" => ("Option".to_string(), name),
                    "Ok" | "Err" => ("Result".to_string(), name),
                    _ => {
                        self.expect(Tok::Dot, "`.` (enum patterns are written `Enum.Variant`)")?;
                        let (variant, _) = self.upper_ident("variant name")?;
                        (name, variant)
                    }
                };
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
            other => Err(self.error(format!("expected a pattern, found {}", other.describe()))),
        }
    }
}

fn binary(op: BinOp, lhs: Expr, rhs: Expr) -> Expr {
    let span = lhs.span.to(rhs.span);
    Expr { kind: ExprKind::Binary(op, Box::new(lhs), Box::new(rhs)), span }
}

fn is_place(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Ident(_) => true,
        ExprKind::Field(base, _, _) => is_place(base),
        ExprKind::Index(base, _) => is_place(base),
        _ => false,
    }
}
