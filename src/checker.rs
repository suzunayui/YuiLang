//! Type checker: types, effects, mutability, unused values and match exhaustiveness.

use crate::ast::*;
use crate::diag::{Diagnostic, Span};
use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq)]
pub enum Type {
    Int,
    Float,
    Bool,
    Str,
    Unit,
    /// Type of expressions that never produce a value (`return`, `break`, `panic(..)`).
    Never,
    /// Produced after an error has been reported; unifies with everything.
    Error,
    List(Box<Type>),
    Option(Box<Type>),
    Result(Box<Type>, Box<Type>),
    Struct(String),
    Enum(String),
    /// Inference variable.
    Var(usize),
}

#[derive(Clone)]
struct FnSig {
    params: Vec<Type>,
    ret: Type,
    effects: Vec<String>,
}

const RESERVED_TYPES: &[&str] =
    &["Int", "Float", "Bool", "String", "Unit", "List", "Option", "Result", "Some", "None", "Ok", "Err"];
const BUILTIN_FNS: &[&str] = &["print", "read_line", "panic"];

struct Checker {
    structs: HashMap<String, Vec<(String, Type)>>,
    enums: HashMap<String, Vec<(String, Vec<Type>)>>,
    fns: HashMap<String, FnSig>,
    subst: Vec<Option<Type>>,
    scopes: Vec<HashMap<String, (Type, bool)>>,
    ret: Type,
    effects: Vec<String>,
    in_contract: bool,
    loop_depth: usize,
    diags: Vec<Diagnostic>,
}

pub fn check(program: &Program) -> Vec<Diagnostic> {
    let mut c = Checker {
        structs: HashMap::new(),
        enums: HashMap::new(),
        fns: HashMap::new(),
        subst: Vec::new(),
        scopes: Vec::new(),
        ret: Type::Unit,
        effects: Vec::new(),
        in_contract: false,
        loop_depth: 0,
        diags: Vec::new(),
    };
    let sigs = c.collect(program);
    let mut sigs = sigs.into_iter();
    for item in &program.items {
        match item {
            Item::Fn(f) => {
                let sig = sigs.next().expect("one signature per function");
                c.check_fn(f, &sig);
            }
            Item::Test(t) => c.check_test(t),
            _ => {}
        }
    }
    c.diags.sort_by_key(|d| d.span.start);
    c.diags
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = vec![i];
        for j in 1..=b.len() {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur.push((prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost));
        }
        prev = cur;
    }
    prev[b.len()]
}

fn closest<'a>(name: &str, candidates: impl Iterator<Item = &'a String>) -> Option<String> {
    candidates
        .map(|c| (levenshtein(name, c), c))
        .filter(|(d, c)| *d <= 2 && *d < name.len().max(c.len()))
        .min_by_key(|(d, _)| *d)
        .map(|(_, c)| c.clone())
}

fn tail_span(block: &Block) -> Span {
    match block.stmts.last() {
        Some(Stmt::Expr(e)) => e.span,
        _ => Span::new(block.span.end.saturating_sub(1), block.span.end),
    }
}

impl Checker {
    fn error(&mut self, code: &'static str, msg: impl Into<String>, span: Span) {
        self.diags.push(Diagnostic::new(code, msg, span));
    }

    fn error_hint(&mut self, code: &'static str, msg: impl Into<String>, span: Span, hint: impl Into<String>) {
        self.diags.push(Diagnostic::new(code, msg, span).hint(hint));
    }

    // ---- types ----

    fn fresh(&mut self) -> Type {
        self.subst.push(None);
        Type::Var(self.subst.len() - 1)
    }

    /// Follows inference variables at the top level only.
    fn resolve(&self, t: &Type) -> Type {
        let mut t = t.clone();
        while let Type::Var(v) = t {
            match &self.subst[v] {
                Some(inner) => t = inner.clone(),
                None => break,
            }
        }
        t
    }

    fn apply(&self, t: &Type) -> Type {
        match self.resolve(t) {
            Type::List(a) => Type::List(Box::new(self.apply(&a))),
            Type::Option(a) => Type::Option(Box::new(self.apply(&a))),
            Type::Result(a, b) => Type::Result(Box::new(self.apply(&a)), Box::new(self.apply(&b))),
            other => other,
        }
    }

    fn show(&self, t: &Type) -> String {
        match self.resolve(t) {
            Type::Int => "Int".to_string(),
            Type::Float => "Float".to_string(),
            Type::Bool => "Bool".to_string(),
            Type::Str => "String".to_string(),
            Type::Unit => "Unit".to_string(),
            Type::Never => "Never".to_string(),
            Type::Error => "?".to_string(),
            Type::List(a) => format!("List<{}>", self.show(&a)),
            Type::Option(a) => format!("Option<{}>", self.show(&a)),
            Type::Result(a, b) => format!("Result<{}, {}>", self.show(&a), self.show(&b)),
            Type::Struct(n) | Type::Enum(n) => n,
            Type::Var(_) => "_".to_string(),
        }
    }

    fn occurs(&self, v: usize, t: &Type) -> bool {
        match self.resolve(t) {
            Type::Var(w) => v == w,
            Type::List(a) | Type::Option(a) => self.occurs(v, &a),
            Type::Result(a, b) => self.occurs(v, &a) || self.occurs(v, &b),
            _ => false,
        }
    }

    fn unify(&mut self, a: &Type, b: &Type) -> bool {
        let a = self.resolve(a);
        let b = self.resolve(b);
        match (&a, &b) {
            (Type::Error, _) | (_, Type::Error) | (Type::Never, _) | (_, Type::Never) => true,
            (Type::Var(x), Type::Var(y)) if x == y => true,
            (Type::Var(x), t) | (t, Type::Var(x)) => {
                if self.occurs(*x, t) {
                    false
                } else {
                    self.subst[*x] = Some(t.clone());
                    true
                }
            }
            (Type::List(x), Type::List(y)) | (Type::Option(x), Type::Option(y)) => self.unify(x, y),
            (Type::Result(x1, x2), Type::Result(y1, y2)) => {
                let first = self.unify(x1, y1);
                let second = self.unify(x2, y2);
                first && second
            }
            _ => a == b,
        }
    }

    fn expect(&mut self, actual: &Type, expected: &Type, span: Span) {
        if !self.unify(actual, expected) {
            let msg = format!(
                "type mismatch: expected `{}`, found `{}`",
                self.show(expected),
                self.show(actual)
            );
            let (e, a) = (self.resolve(expected), self.resolve(actual));
            let hint = match (&e, &a) {
                (Type::Float, Type::Int) => Some("convert explicitly with `.to_float()`, or write a Float literal like `1.0`"),
                (Type::Int, Type::Float) => Some("convert explicitly with `.to_int()`"),
                (Type::Str, _) => Some("convert with `.to_string()` or interpolate: \"{value}\""),
                (Type::Option(_), _) if !matches!(a, Type::Option(_)) => Some("wrap the value in `Some(...)`"),
                (Type::Result(_, _), _) if !matches!(a, Type::Result(_, _)) => Some("wrap the value in `Ok(...)` or `Err(...)`"),
                (_, Type::Option(_)) => Some("handle the `None` case with `match`, `?` or `.unwrap_or(default)`"),
                (_, Type::Result(_, _)) => Some("handle the `Err` case with `match`, `?` or `.unwrap_or(default)`"),
                _ => None,
            };
            let mut d = Diagnostic::new("E0301", msg, span);
            if let Some(h) = hint {
                d = d.hint(h);
            }
            self.diags.push(d);
        }
    }

    /// Type of a value produced by two alternative branches.
    fn join(&mut self, a: &Type, b: &Type, span: Span) -> Type {
        if self.resolve(a) == Type::Never {
            return b.clone();
        }
        if self.resolve(b) == Type::Never {
            return a.clone();
        }
        if !self.unify(a, b) {
            let msg = format!(
                "branches have different types: `{}` and `{}`",
                self.show(a),
                self.show(b)
            );
            self.error("E0301", msg, span);
        }
        a.clone()
    }

    fn resolve_type(&mut self, t: &TypeExpr) -> Type {
        let name = t.name.as_str();
        let arity = match name {
            "Int" | "Float" | "Bool" | "String" | "Unit" => 0,
            "List" | "Option" => 1,
            "Result" => 2,
            _ if self.structs.contains_key(name) || self.enums.contains_key(name) => 0,
            _ => {
                let known = self.structs.keys().chain(self.enums.keys());
                let suggestion = closest(name, known);
                let mut d = Diagnostic::new("E0202", format!("unknown type `{name}`"), t.span);
                if let Some(s) = suggestion {
                    d = d.hint(format!("did you mean `{s}`?"));
                }
                self.diags.push(d);
                return Type::Error;
            }
        };
        if t.args.len() != arity {
            self.error(
                "E0206",
                format!("type `{name}` takes {arity} type argument(s), found {}", t.args.len()),
                t.span,
            );
            return Type::Error;
        }
        let mut args: Vec<Type> = t.args.iter().map(|a| self.resolve_type(a)).collect();
        match name {
            "Int" => Type::Int,
            "Float" => Type::Float,
            "Bool" => Type::Bool,
            "String" => Type::Str,
            "Unit" => Type::Unit,
            "List" => Type::List(Box::new(args.remove(0))),
            "Option" => Type::Option(Box::new(args.remove(0))),
            "Result" => {
                let ok = args.remove(0);
                let err = args.remove(0);
                Type::Result(Box::new(ok), Box::new(err))
            }
            _ if self.structs.contains_key(name) => Type::Struct(name.to_string()),
            _ => Type::Enum(name.to_string()),
        }
    }

    // ---- declarations ----

    /// Registers all declarations. Returns one signature per `fn` item, in source order.
    fn collect(&mut self, program: &Program) -> Vec<FnSig> {
        for item in &program.items {
            let (name, span) = match item {
                Item::Struct(s) => (&s.name, s.name_span),
                Item::Enum(e) => (&e.name, e.name_span),
                _ => continue,
            };
            if RESERVED_TYPES.contains(&name.as_str()) {
                self.error("E0201", format!("`{name}` is a built-in name and cannot be redefined"), span);
            } else if self.structs.contains_key(name) || self.enums.contains_key(name) {
                self.error("E0201", format!("type `{name}` is defined more than once"), span);
            } else if matches!(item, Item::Struct(_)) {
                self.structs.insert(name.clone(), Vec::new());
            } else {
                self.enums.insert(name.clone(), Vec::new());
            }
        }

        for item in &program.items {
            match item {
                Item::Struct(s) => {
                    let mut fields: Vec<(String, Type)> = Vec::new();
                    for f in &s.fields {
                        let ty = self.resolve_type(&f.ty);
                        if fields.iter().any(|(n, _)| n == &f.name) {
                            self.error("E0201", format!("field `{}` is defined more than once", f.name), f.span);
                        } else {
                            fields.push((f.name.clone(), ty));
                        }
                    }
                    if let Some(slot) = self.structs.get_mut(&s.name) {
                        if slot.is_empty() {
                            *slot = fields;
                        }
                    }
                }
                Item::Enum(e) => {
                    let mut variants: Vec<(String, Vec<Type>)> = Vec::new();
                    for v in &e.variants {
                        let tys: Vec<Type> = v.fields.iter().map(|t| self.resolve_type(t)).collect();
                        if variants.iter().any(|(n, _)| n == &v.name) {
                            self.error("E0201", format!("variant `{}` is defined more than once", v.name), v.span);
                        } else {
                            variants.push((v.name.clone(), tys));
                        }
                    }
                    if let Some(slot) = self.enums.get_mut(&e.name) {
                        if slot.is_empty() {
                            *slot = variants;
                        }
                    }
                }
                _ => {}
            }
        }

        let mut sigs = Vec::new();
        for item in &program.items {
            let Item::Fn(f) = item else { continue };
            let mut seen: Vec<&str> = Vec::new();
            let mut params = Vec::new();
            for p in &f.params {
                if seen.contains(&p.name.as_str()) {
                    self.error("E0201", format!("parameter `{}` is defined more than once", p.name), p.span);
                }
                seen.push(&p.name);
                params.push(self.resolve_type(&p.ty));
            }
            let ret = match &f.ret {
                Some(t) => self.resolve_type(t),
                None => Type::Unit,
            };
            let effects: Vec<String> = f.effects.iter().map(|(e, _)| e.clone()).collect();
            let sig = FnSig { params, ret, effects };
            if BUILTIN_FNS.contains(&f.name.as_str()) {
                self.error(
                    "E0201",
                    format!("`{}` is a built-in function and cannot be redefined", f.name),
                    f.name_span,
                );
            } else if self.fns.contains_key(&f.name) {
                self.error("E0201", format!("function `{}` is defined more than once", f.name), f.name_span);
            } else {
                self.fns.insert(f.name.clone(), sig.clone());
            }
            sigs.push(sig);
        }
        sigs
    }

    fn check_fn(&mut self, f: &FnDecl, sig: &FnSig) {
        self.scopes = vec![HashMap::new()];
        self.ret = sig.ret.clone();
        self.loop_depth = 0;
        for (p, ty) in f.params.iter().zip(&sig.params) {
            self.scopes[0].insert(p.name.clone(), (ty.clone(), false));
        }

        // Contracts must be pure.
        self.effects = Vec::new();
        self.in_contract = true;
        for r in &f.requires {
            let t = self.expr(r, true);
            self.expect(&t, &Type::Bool, r.span);
        }
        self.scopes.push(HashMap::new());
        self.scopes[1].insert("result".to_string(), (sig.ret.clone(), false));
        for e in &f.ensures {
            let t = self.expr(e, true);
            self.expect(&t, &Type::Bool, e.span);
        }
        self.scopes.pop();
        self.in_contract = false;

        self.effects = sig.effects.clone();
        let want = self.resolve(&sig.ret) != Type::Unit;
        let body_ty = self.block(&f.body, want);
        if want {
            if self.resolve(&body_ty) == Type::Unit {
                let msg = format!(
                    "function `{}` must return `{}`, but its body ends without a value",
                    f.name,
                    self.show(&sig.ret)
                );
                self.error_hint(
                    "E0301",
                    msg,
                    tail_span(&f.body),
                    "make the last line of the body an expression of the return type, or use `return`",
                );
            } else {
                self.expect(&body_ty, &sig.ret, tail_span(&f.body));
            }
        }
    }

    fn check_test(&mut self, t: &TestDecl) {
        self.scopes = vec![HashMap::new()];
        self.ret = Type::Unit;
        self.loop_depth = 0;
        self.effects = t.effects.iter().map(|(e, _)| e.clone()).collect();
        self.block(&t.body, false);
    }

    // ---- scopes ----

    fn lookup(&self, name: &str) -> Option<(Type, bool)> {
        self.scopes.iter().rev().find_map(|s| s.get(name).cloned())
    }

    fn declare(&mut self, name: &str, ty: Type, mutable: bool, span: Span) {
        if name == "_" {
            return;
        }
        let scope = self.scopes.last_mut().expect("scope");
        if scope.contains_key(name) {
            self.error_hint(
                "E0204",
                format!("`{name}` is already defined in this scope"),
                span,
                "choose a different name, or declare the first one with `let mut` and assign to it",
            );
            return;
        }
        scope.insert(name.to_string(), (ty, mutable));
    }

    fn undefined_variable(&mut self, name: &str, span: Span) {
        let names: Vec<String> = self.scopes.iter().flat_map(|s| s.keys().cloned()).collect();
        let mut d = Diagnostic::new("E0203", format!("undefined variable `{name}`"), span);
        if let Some(s) = closest(name, names.iter()) {
            d = d.hint(format!("did you mean `{s}`?"));
        }
        self.diags.push(d);
    }

    // ---- statements ----

    /// Checks a block. When `want` is true the block's value (its final
    /// expression) is used and its type is returned; otherwise `Unit`.
    fn block(&mut self, block: &Block, want: bool) -> Type {
        self.scopes.push(HashMap::new());
        let mut ty = Type::Unit;
        let last = block.stmts.len().saturating_sub(1);
        for (i, stmt) in block.stmts.iter().enumerate() {
            match stmt {
                Stmt::Expr(e) if want && i == last => ty = self.expr(e, true),
                _ => self.stmt(stmt),
            }
        }
        self.scopes.pop();
        ty
    }

    /// Checks an expression whose value is discarded.
    fn expr_stmt(&mut self, e: &Expr) {
        let t = self.expr(e, false);
        match self.resolve(&t) {
            Type::Unit | Type::Never | Type::Error => {}
            _ => {
                let msg = format!("unused value of type `{}`", self.show(&t));
                self.error_hint(
                    "E0305",
                    msg,
                    e.span,
                    "assign the result, or discard it explicitly with `let _ = ...`",
                );
            }
        }
    }

    fn stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Let { name, name_span, mutable, ty, value } => {
                let value_ty = self.expr(value, true);
                let declared = match ty {
                    Some(t) => {
                        let annotated = self.resolve_type(t);
                        self.expect(&value_ty, &annotated, value.span);
                        annotated
                    }
                    None => value_ty,
                };
                self.declare(name, declared, *mutable, *name_span);
            }
            Stmt::Assign { target, value } => {
                let target_ty = self.expr(target, true);
                let mut root = target;
                loop {
                    match &root.kind {
                        ExprKind::Field(base, _, _) | ExprKind::Index(base, _) => root = base,
                        _ => break,
                    }
                }
                if let ExprKind::Ident(name) = &root.kind {
                    if let Some((_, false)) = self.lookup(name) {
                        self.error_hint(
                            "E0306",
                            format!("cannot assign to `{name}` because it is not mutable"),
                            target.span,
                            format!("declare it with `let mut {name} = ...`"),
                        );
                    }
                }
                let value_ty = self.expr(value, true);
                self.expect(&value_ty, &target_ty, value.span);
            }
            Stmt::While { cond, body } => {
                let t = self.expr(cond, true);
                self.expect(&t, &Type::Bool, cond.span);
                self.loop_depth += 1;
                self.block(body, false);
                self.loop_depth -= 1;
            }
            Stmt::For { var, var_span, iter, end, body } => {
                let iter_ty = self.expr(iter, true);
                let elem = match end {
                    Some(end) => {
                        self.expect(&iter_ty, &Type::Int, iter.span);
                        let end_ty = self.expr(end, true);
                        self.expect(&end_ty, &Type::Int, end.span);
                        Type::Int
                    }
                    None => {
                        let elem = self.fresh();
                        let list = Type::List(Box::new(elem.clone()));
                        if !self.unify(&iter_ty, &list) {
                            let msg = format!("cannot iterate over a value of type `{}`", self.show(&iter_ty));
                            self.error_hint(
                                "E0301",
                                msg,
                                iter.span,
                                "`for` iterates over a List, or over a range written `start..end`",
                            );
                        }
                        elem
                    }
                };
                self.scopes.push(HashMap::new());
                self.declare(var, elem, false, *var_span);
                self.loop_depth += 1;
                self.block(body, false);
                self.loop_depth -= 1;
                self.scopes.pop();
            }
            Stmt::Assert { cond, .. } => {
                let t = self.expr(cond, true);
                self.expect(&t, &Type::Bool, cond.span);
            }
            Stmt::Expr(e) => self.expr_stmt(e),
        }
    }

    // ---- expressions ----

    fn require_effects(&mut self, callee: &str, needed: &[String], span: Span) {
        for eff in needed {
            if !self.effects.contains(eff) {
                let hint = if self.in_contract {
                    "`requires` and `ensures` clauses must be free of side effects".to_string()
                } else {
                    format!("add `uses {eff}` to the signature of the enclosing function or test")
                };
                self.error_hint(
                    "E0401",
                    format!("`{callee}` uses the `{eff}` effect, which is not declared here"),
                    span,
                    hint,
                );
            }
        }
    }

    fn check_args(&mut self, what: &str, params: &[Type], args: &[Expr], span: Span) {
        if params.len() != args.len() {
            self.error(
                "E0303",
                format!("{what} takes {} argument(s), but {} were given", params.len(), args.len()),
                span,
            );
        }
        for (i, arg) in args.iter().enumerate() {
            let t = self.expr(arg, true);
            if let Some(p) = params.get(i) {
                self.expect(&t, p, arg.span);
            }
        }
    }

    /// `want` is false when the value of the expression is discarded.
    fn expr(&mut self, e: &Expr, want: bool) -> Type {
        match &e.kind {
            ExprKind::Int(_) => Type::Int,
            ExprKind::Float(_) => Type::Float,
            ExprKind::Bool(_) => Type::Bool,
            ExprKind::Str(parts) => {
                for part in parts {
                    if let StrPart::Expr(inner) = part {
                        self.expr(inner, true);
                    }
                }
                Type::Str
            }
            ExprKind::Ident(name) => match self.lookup(name) {
                Some((ty, _)) => ty,
                None => {
                    self.undefined_variable(name, e.span);
                    Type::Error
                }
            },
            ExprKind::List(items) => {
                let elem = self.fresh();
                for item in items {
                    let t = self.expr(item, true);
                    self.expect(&t, &elem, item.span);
                }
                Type::List(Box::new(elem))
            }
            ExprKind::Unary(op, operand) => {
                let t = self.expr(operand, true);
                match op {
                    UnOp::Not => {
                        self.expect(&t, &Type::Bool, operand.span);
                        Type::Bool
                    }
                    UnOp::Neg => match self.resolve(&t) {
                        Type::Int | Type::Float | Type::Error | Type::Never => t,
                        _ => {
                            let msg = format!("cannot negate a value of type `{}`", self.show(&t));
                            self.error("E0302", msg, e.span);
                            Type::Error
                        }
                    },
                }
            }
            ExprKind::Binary(op, lhs, rhs) => self.binary(*op, lhs, rhs, e.span),
            ExprKind::Call { name, name_span, args } => self.call(name, *name_span, args, e.span),
            ExprKind::MethodCall { recv, name, name_span, args } => {
                let recv_ty = self.expr(recv, true);
                self.method(&recv_ty, name, *name_span, args, e.span)
            }
            ExprKind::Field(base, name, name_span) => {
                let base_ty = self.expr(base, true);
                match self.resolve(&base_ty) {
                    Type::Struct(sname) => {
                        let fields = self.structs.get(&sname).cloned().unwrap_or_default();
                        match fields.iter().find(|(n, _)| n == name) {
                            Some((_, ty)) => ty.clone(),
                            None => {
                                let names: Vec<String> = fields.iter().map(|(n, _)| n.clone()).collect();
                                let mut d = Diagnostic::new(
                                    "E0304",
                                    format!("struct `{sname}` has no field `{name}`"),
                                    *name_span,
                                );
                                d = match closest(name, names.iter()) {
                                    Some(s) => d.hint(format!("did you mean `{s}`?")),
                                    None => d.hint(format!("fields of `{sname}`: {}", names.join(", "))),
                                };
                                self.diags.push(d);
                                Type::Error
                            }
                        }
                    }
                    Type::Error | Type::Never => Type::Error,
                    Type::Var(_) => {
                        self.error_hint(
                            "E0311",
                            "the type of this value is not known yet",
                            base.span,
                            "add a type annotation where the value is declared",
                        );
                        Type::Error
                    }
                    _ => {
                        let msg = format!("type `{}` has no field `{name}`", self.show(&base_ty));
                        self.error("E0304", msg, *name_span);
                        Type::Error
                    }
                }
            }
            ExprKind::Index(base, index) => {
                let base_ty = self.expr(base, true);
                let index_ty = self.expr(index, true);
                self.expect(&index_ty, &Type::Int, index.span);
                let elem = self.fresh();
                let list = Type::List(Box::new(elem.clone()));
                if !self.unify(&base_ty, &list) {
                    let msg = format!("cannot index into a value of type `{}`", self.show(&base_ty));
                    self.error("E0302", msg, base.span);
                    return Type::Error;
                }
                elem
            }
            ExprKind::StructLit { name, fields } => {
                let Some(decl) = self.structs.get(name).cloned() else {
                    let mut d = Diagnostic::new("E0202", format!("unknown struct `{name}`"), e.span);
                    if let Some(s) = closest(name, self.structs.keys()) {
                        d = d.hint(format!("did you mean `{s}`?"));
                    }
                    self.diags.push(d);
                    for (_, _, value) in fields {
                        self.expr(value, true);
                    }
                    return Type::Error;
                };
                let mut given: Vec<&str> = Vec::new();
                for (fname, fspan, value) in fields {
                    let t = self.expr(value, true);
                    if given.contains(&fname.as_str()) {
                        self.error("E0201", format!("field `{fname}` is given more than once"), *fspan);
                        continue;
                    }
                    given.push(fname);
                    match decl.iter().find(|(n, _)| n == fname) {
                        Some((_, ty)) => self.expect(&t, ty, value.span),
                        None => {
                            let names: Vec<String> = decl.iter().map(|(n, _)| n.clone()).collect();
                            self.error_hint(
                                "E0304",
                                format!("struct `{name}` has no field `{fname}`"),
                                *fspan,
                                format!("fields of `{name}`: {}", names.join(", ")),
                            );
                        }
                    }
                }
                let missing: Vec<&str> = decl
                    .iter()
                    .map(|(n, _)| n.as_str())
                    .filter(|n| !given.contains(n))
                    .collect();
                if !missing.is_empty() {
                    self.error(
                        "E0312",
                        format!("missing field(s) in `{name}`: {}", missing.join(", ")),
                        e.span,
                    );
                }
                Type::Struct(name.clone())
            }
            ExprKind::Ctor { enum_name, variant, args } => self.ctor(enum_name, variant, args, e.span),
            ExprKind::If { cond, then, els } => {
                let t = self.expr(cond, true);
                self.expect(&t, &Type::Bool, cond.span);
                match els {
                    Some(els) if want => {
                        let then_ty = self.block(then, true);
                        let else_ty = self.expr(els, true);
                        self.join(&then_ty, &else_ty, e.span)
                    }
                    Some(els) => {
                        self.block(then, false);
                        self.expr(els, false);
                        Type::Unit
                    }
                    None => {
                        self.block(then, false);
                        Type::Unit
                    }
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                let scrut_ty = self.expr(scrutinee, true);
                let mut result: Option<Type> = None;
                for arm in arms {
                    self.scopes.push(HashMap::new());
                    self.pattern(&arm.pat, &scrut_ty);
                    if want {
                        let t = self.expr(&arm.body, true);
                        result = Some(match result {
                            Some(prev) => self.join(&prev, &t, arm.body.span),
                            None => t,
                        });
                    } else {
                        self.expr_stmt(&arm.body);
                    }
                    self.scopes.pop();
                }
                let pats: Vec<&Pattern> = arms.iter().map(|a| &a.pat).collect();
                if let Some(missing) = self.missing(&pats, &scrut_ty) {
                    self.error_hint(
                        "E0308",
                        format!("match is not exhaustive: `{missing}` is not covered"),
                        scrutinee.span,
                        format!("add an arm for `{missing}`, or a catch-all arm `_ => ...`"),
                    );
                }
                if want {
                    result.unwrap_or(Type::Never)
                } else {
                    Type::Unit
                }
            }
            ExprKind::Block(b) => self.block(b, want),
            ExprKind::Return(value) => {
                let ret = self.ret.clone();
                match value {
                    Some(v) => {
                        let t = self.expr(v, true);
                        self.expect(&t, &ret, v.span);
                    }
                    None => self.expect(&Type::Unit, &ret, e.span),
                }
                Type::Never
            }
            ExprKind::Break | ExprKind::Continue => {
                if self.loop_depth == 0 {
                    self.error("E0309", "`break` and `continue` can only be used inside a loop", e.span);
                }
                Type::Never
            }
            ExprKind::Try(inner) => {
                let t = self.expr(inner, true);
                let ret = self.ret.clone();
                match self.resolve(&t) {
                    Type::Result(ok, err) => {
                        let expected = Type::Result(Box::new(self.fresh()), err);
                        if !self.unify(&ret, &expected) {
                            let msg = format!(
                                "`?` on a Result needs the enclosing function to return `{}`, but it returns `{}`",
                                self.show(&expected),
                                self.show(&ret)
                            );
                            self.error("E0310", msg, e.span);
                        }
                        *ok
                    }
                    Type::Option(some) => {
                        let expected = Type::Option(Box::new(self.fresh()));
                        if !self.unify(&ret, &expected) {
                            let msg = format!(
                                "`?` on an Option needs the enclosing function to return an Option, but it returns `{}`",
                                self.show(&ret)
                            );
                            self.error("E0310", msg, e.span);
                        }
                        *some
                    }
                    Type::Error | Type::Never => Type::Error,
                    _ => {
                        let msg = format!(
                            "`?` can only be applied to a Result or an Option, found `{}`",
                            self.show(&t)
                        );
                        self.error("E0310", msg, e.span);
                        Type::Error
                    }
                }
            }
        }
    }

    fn binary(&mut self, op: BinOp, lhs: &Expr, rhs: &Expr, span: Span) -> Type {
        let l = self.expr(lhs, true);
        let r = self.expr(rhs, true);
        match op {
            BinOp::And | BinOp::Or => {
                self.expect(&l, &Type::Bool, lhs.span);
                self.expect(&r, &Type::Bool, rhs.span);
                Type::Bool
            }
            BinOp::Eq | BinOp::Ne => {
                self.expect(&r, &l, rhs.span);
                Type::Bool
            }
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                self.expect(&r, &l, rhs.span);
                match self.resolve(&l) {
                    Type::Int | Type::Float | Type::Str | Type::Error | Type::Never => {}
                    _ => {
                        let msg = format!(
                            "`{}` cannot compare values of type `{}`",
                            op.symbol(),
                            self.show(&l)
                        );
                        self.error_hint("E0302", msg, span, "ordering works on Int, Float and String");
                    }
                }
                Type::Bool
            }
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => {
                let lt = self.resolve(&l);
                match lt {
                    Type::Int | Type::Float => {
                        self.expect(&r, &l, rhs.span);
                        l
                    }
                    Type::Error | Type::Never => Type::Error,
                    Type::Str if op == BinOp::Add => {
                        self.error_hint(
                            "E0302",
                            "`+` cannot join strings",
                            span,
                            "use interpolation instead: \"{a}{b}\"",
                        );
                        Type::Str
                    }
                    _ => {
                        let msg = format!(
                            "`{}` cannot be applied to a value of type `{}`",
                            op.symbol(),
                            self.show(&l)
                        );
                        self.error_hint("E0302", msg, lhs.span, "arithmetic works on Int and Float");
                        Type::Error
                    }
                }
            }
        }
    }

    fn call(&mut self, name: &str, name_span: Span, args: &[Expr], span: Span) -> Type {
        let builtin: Option<(Vec<Type>, Type, &[&str])> = match name {
            "print" => Some((vec![Type::Str], Type::Unit, &["io"])),
            "read_line" => Some((vec![], Type::Str, &["io"])),
            "panic" => Some((vec![Type::Str], Type::Never, &[])),
            _ => None,
        };
        let (params, ret, effects) = match builtin {
            Some((params, ret, effects)) => (params, ret, effects.iter().map(|s| s.to_string()).collect()),
            None => match self.fns.get(name) {
                Some(sig) => (sig.params.clone(), sig.ret.clone(), sig.effects.clone()),
                None => {
                    let mut names: Vec<String> = self.fns.keys().cloned().collect();
                    names.extend(BUILTIN_FNS.iter().map(|s| s.to_string()));
                    let mut d = Diagnostic::new("E0203", format!("undefined function `{name}`"), name_span);
                    if let Some(s) = closest(name, names.iter()) {
                        d = d.hint(format!("did you mean `{s}`?"));
                    }
                    self.diags.push(d);
                    for arg in args {
                        self.expr(arg, true);
                    }
                    return Type::Error;
                }
            },
        };
        self.require_effects(name, &effects, span);
        self.check_args(&format!("`{name}`"), &params, args, span);
        ret
    }

    fn method(&mut self, recv_ty: &Type, name: &str, name_span: Span, args: &[Expr], span: Span) -> Type {
        let recv = self.resolve(recv_ty);
        let boxed = |t: &Type| Box::new(t.clone());
        let sig: Option<(Vec<Type>, Type)> = match (&recv, name) {
            (Type::Error | Type::Never, _) => {
                for arg in args {
                    self.expr(arg, true);
                }
                return Type::Error;
            }
            (_, "to_string") => Some((vec![], Type::Str)),
            (Type::Var(_), _) => {
                self.error_hint(
                    "E0311",
                    "the type of this value is not known yet",
                    span,
                    "add a type annotation where the value is declared",
                );
                for arg in args {
                    self.expr(arg, true);
                }
                return Type::Error;
            }
            (Type::Str, "len") => Some((vec![], Type::Int)),
            (Type::Str, "contains") => Some((vec![Type::Str], Type::Bool)),
            (Type::Str, "trim") => Some((vec![], Type::Str)),
            (Type::Str, "to_int") => Some((vec![], Type::Option(Box::new(Type::Int)))),
            (Type::Str, "to_float") => Some((vec![], Type::Option(Box::new(Type::Float)))),
            (Type::List(_), "len") => Some((vec![], Type::Int)),
            (Type::List(t), "push") => Some((vec![(**t).clone()], Type::List(boxed(t)))),
            (Type::List(t), "get") => Some((vec![Type::Int], Type::Option(boxed(t)))),
            (Type::List(t), "contains") => Some((vec![(**t).clone()], Type::Bool)),
            (Type::Int, "to_float") => Some((vec![], Type::Float)),
            (Type::Float, "to_int") => Some((vec![], Type::Int)),
            (Type::Option(t), "unwrap_or") | (Type::Result(t, _), "unwrap_or") => {
                Some((vec![(**t).clone()], (**t).clone()))
            }
            (Type::Option(_), "is_some" | "is_none") => Some((vec![], Type::Bool)),
            (Type::Result(_, _), "is_ok" | "is_err") => Some((vec![], Type::Bool)),
            _ => None,
        };
        match sig {
            Some((params, ret)) => {
                self.check_args(&format!("method `{name}`"), &params, args, span);
                ret
            }
            None => {
                let available = match &recv {
                    Type::Str => "len, contains, trim, to_int, to_float, to_string",
                    Type::List(_) => "len, push, get, contains, to_string",
                    Type::Int => "to_float, to_string",
                    Type::Float => "to_int, to_string",
                    Type::Option(_) => "unwrap_or, is_some, is_none, to_string",
                    Type::Result(_, _) => "unwrap_or, is_ok, is_err, to_string",
                    _ => "to_string",
                };
                let msg = format!("type `{}` has no method `{name}`", self.show(&recv));
                self.error_hint("E0304", msg, name_span, format!("available methods: {available}"));
                for arg in args {
                    self.expr(arg, true);
                }
                Type::Error
            }
        }
    }

    fn ctor(&mut self, enum_name: &str, variant: &str, args: &[Expr], span: Span) -> Type {
        match (enum_name, variant) {
            ("Option", "Some") => {
                let t = self.expr(&args[0], true);
                Type::Option(Box::new(t))
            }
            ("Option", _) => Type::Option(Box::new(self.fresh())),
            ("Result", "Ok") => {
                let t = self.expr(&args[0], true);
                Type::Result(Box::new(t), Box::new(self.fresh()))
            }
            ("Result", _) => {
                let t = self.expr(&args[0], true);
                Type::Result(Box::new(self.fresh()), Box::new(t))
            }
            _ => {
                let Some(variants) = self.enums.get(enum_name).cloned() else {
                    let mut d = Diagnostic::new("E0202", format!("unknown enum `{enum_name}`"), span);
                    if let Some(s) = closest(enum_name, self.enums.keys()) {
                        d = d.hint(format!("did you mean `{s}`?"));
                    }
                    self.diags.push(d);
                    for arg in args {
                        self.expr(arg, true);
                    }
                    return Type::Error;
                };
                match variants.iter().find(|(n, _)| n == variant) {
                    Some((_, params)) => {
                        self.check_args(&format!("`{enum_name}.{variant}`"), params, args, span);
                    }
                    None => {
                        let names: Vec<String> = variants.iter().map(|(n, _)| n.clone()).collect();
                        self.error_hint(
                            "E0304",
                            format!("enum `{enum_name}` has no variant `{variant}`"),
                            span,
                            format!("variants of `{enum_name}`: {}", names.join(", ")),
                        );
                        for arg in args {
                            self.expr(arg, true);
                        }
                    }
                }
                Type::Enum(enum_name.to_string())
            }
        }
    }

    // ---- patterns ----

    fn pattern(&mut self, pat: &Pattern, ty: &Type) {
        match &pat.kind {
            PatKind::Wildcard => {}
            PatKind::Bind(name) => self.declare(name, ty.clone(), false, pat.span),
            PatKind::Int(_) => self.expect_pattern(&Type::Int, ty, pat.span),
            PatKind::Str(_) => self.expect_pattern(&Type::Str, ty, pat.span),
            PatKind::Bool(_) => self.expect_pattern(&Type::Bool, ty, pat.span),
            PatKind::Ctor { enum_name, variant, args } => {
                let (pat_ty, params): (Type, Vec<Type>) = match (enum_name.as_str(), variant.as_str()) {
                    ("Option", "Some") => {
                        let t = self.fresh();
                        (Type::Option(Box::new(t.clone())), vec![t])
                    }
                    ("Option", _) => (Type::Option(Box::new(self.fresh())), vec![]),
                    ("Result", "Ok") => {
                        let t = self.fresh();
                        (Type::Result(Box::new(t.clone()), Box::new(self.fresh())), vec![t])
                    }
                    ("Result", _) => {
                        let t = self.fresh();
                        (Type::Result(Box::new(self.fresh()), Box::new(t.clone())), vec![t])
                    }
                    _ => {
                        let Some(variants) = self.enums.get(enum_name).cloned() else {
                            self.error("E0202", format!("unknown enum `{enum_name}`"), pat.span);
                            return;
                        };
                        match variants.into_iter().find(|(n, _)| n == variant) {
                            Some((_, params)) => (Type::Enum(enum_name.clone()), params),
                            None => {
                                self.error(
                                    "E0304",
                                    format!("enum `{enum_name}` has no variant `{variant}`"),
                                    pat.span,
                                );
                                return;
                            }
                        }
                    }
                };
                self.expect_pattern(&pat_ty, ty, pat.span);
                if params.len() != args.len() {
                    self.error(
                        "E0303",
                        format!(
                            "pattern `{variant}` takes {} field(s), but {} were given",
                            params.len(),
                            args.len()
                        ),
                        pat.span,
                    );
                    return;
                }
                for (arg, param) in args.iter().zip(&params) {
                    self.pattern(arg, param);
                }
            }
        }
    }

    fn expect_pattern(&mut self, pat_ty: &Type, value_ty: &Type, span: Span) {
        if !self.unify(pat_ty, value_ty) {
            let msg = format!(
                "this pattern matches `{}`, but the matched value has type `{}`",
                self.show(pat_ty),
                self.show(value_ty)
            );
            self.error("E0301", msg, span);
        }
    }

    /// Returns a description of a value not covered by `pats`, or None when the
    /// patterns are exhaustive. Conservative: it may reject an exhaustive match
    /// that relies on combining several multi-field patterns, but it never
    /// accepts a non-exhaustive one.
    fn missing(&self, pats: &[&Pattern], ty: &Type) -> Option<String> {
        if pats.iter().any(|p| p.is_irrefutable()) {
            return None;
        }
        let sub = |variant: &str| -> Vec<&Pattern> {
            pats.iter()
                .filter_map(|p| match &p.kind {
                    PatKind::Ctor { variant: v, args, .. } if v == variant => args.first(),
                    _ => None,
                })
                .collect()
        };
        let has = |variant: &str| {
            pats.iter().any(|p| matches!(&p.kind, PatKind::Ctor { variant: v, .. } if v == variant))
        };
        match self.apply(ty) {
            Type::Bool => {
                for value in [true, false] {
                    if !pats.iter().any(|p| matches!(p.kind, PatKind::Bool(b) if b == value)) {
                        return Some(value.to_string());
                    }
                }
                None
            }
            Type::Option(inner) => {
                if !has("None") {
                    return Some("None".to_string());
                }
                self.missing(&sub("Some"), &inner).map(|m| format!("Some({m})"))
            }
            Type::Result(ok, err) => {
                if let Some(m) = self.missing(&sub("Ok"), &ok) {
                    return Some(format!("Ok({m})"));
                }
                self.missing(&sub("Err"), &err).map(|m| format!("Err({m})"))
            }
            Type::Enum(name) => {
                let variants = self.enums.get(&name)?;
                for (variant, fields) in variants {
                    match fields.len() {
                        0 => {
                            if !has(variant) {
                                return Some(format!("{name}.{variant}"));
                            }
                        }
                        1 => {
                            if let Some(m) = self.missing(&sub(variant), &fields[0]) {
                                return Some(format!("{name}.{variant}({m})"));
                            }
                        }
                        n => {
                            let covered = pats.iter().any(|p| match &p.kind {
                                PatKind::Ctor { variant: v, args, .. } => {
                                    v == variant && args.iter().all(|a| a.is_irrefutable())
                                }
                                _ => false,
                            });
                            if !covered {
                                return Some(format!("{name}.{variant}({})", vec!["_"; n].join(", ")));
                            }
                        }
                    }
                }
                None
            }
            Type::Error | Type::Never => None,
            _ => Some("_".to_string()),
        }
    }
}
