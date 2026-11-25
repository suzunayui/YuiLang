//! Tree-walking interpreter. Runs programs that have passed the type checker.

use crate::ast::*;
use crate::diag::{Diagnostic, Span};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::rc::Rc;

/// Maximum depth of nested function calls.
pub const MAX_DEPTH: usize = 10_000;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(Rc<str>),
    Unit,
    List(Rc<Vec<Value>>),
    Struct(Rc<StructVal>),
    Enum(Rc<EnumVal>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct StructVal {
    pub name: String,
    pub fields: Vec<(String, Value)>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EnumVal {
    pub enum_name: String,
    pub variant: String,
    pub args: Vec<Value>,
}

impl Value {
    fn str(s: impl AsRef<str>) -> Value {
        Value::Str(Rc::from(s.as_ref()))
    }

    fn variant(enum_name: &str, variant: &str, args: Vec<Value>) -> Value {
        Value::Enum(Rc::new(EnumVal { enum_name: enum_name.to_string(), variant: variant.to_string(), args }))
    }

    fn some(v: Value) -> Value {
        Value::variant("Option", "Some", vec![v])
    }

    fn none() -> Value {
        Value::variant("Option", "None", vec![])
    }

    /// Text used by string interpolation and `to_string()`.
    pub fn display(&self) -> String {
        match self {
            Value::Str(s) => s.to_string(),
            other => other.repr(),
        }
    }

    /// Like `display`, but strings are quoted. Used inside containers and in messages.
    pub fn repr(&self) -> String {
        match self {
            Value::Int(n) => n.to_string(),
            Value::Float(f) => {
                if f.is_finite() && f.fract() == 0.0 && f.abs() < 1e16 {
                    format!("{f:.1}")
                } else {
                    f.to_string()
                }
            }
            Value::Bool(b) => b.to_string(),
            Value::Str(s) => format!("{:?}", &**s),
            Value::Unit => "()".to_string(),
            Value::List(items) => {
                let parts: Vec<String> = items.iter().map(|v| v.repr()).collect();
                format!("[{}]", parts.join(", "))
            }
            Value::Struct(s) => {
                let parts: Vec<String> = s.fields.iter().map(|(n, v)| format!("{n}: {}", v.repr())).collect();
                format!("{} {{ {} }}", s.name, parts.join(", "))
            }
            Value::Enum(e) => {
                let mut out = match e.enum_name.as_str() {
                    "Option" | "Result" => e.variant.clone(),
                    name => format!("{name}.{}", e.variant),
                };
                if !e.args.is_empty() {
                    let parts: Vec<String> = e.args.iter().map(|v| v.repr()).collect();
                    out.push_str(&format!("({})", parts.join(", ")));
                }
                out
            }
        }
    }
}

enum Ctrl {
    Return(Value),
    Break,
    Continue,
    Panic(Box<Diagnostic>),
}

type R<T> = Result<T, Ctrl>;

fn panic(code: &'static str, msg: impl Into<String>, span: Span) -> Ctrl {
    Ctrl::Panic(Box::new(Diagnostic::new(code, msg, span)))
}

fn panic_hint(code: &'static str, msg: impl Into<String>, span: Span, hint: &str) -> Ctrl {
    Ctrl::Panic(Box::new(Diagnostic::new(code, msg, span).hint(hint)))
}

fn internal(span: Span) -> Ctrl {
    panic("E0599", "internal error: the interpreter reached a state the type checker should prevent", span)
}

fn out_of_bounds(index: i64, len: usize, span: Span) -> Ctrl {
    panic_hint(
        "E0505",
        format!("index {index} is out of bounds for a list of length {len}"),
        span,
        "use `.get(i)` to receive an Option instead of failing",
    )
}

pub struct Interp<'a> {
    src: &'a str,
    fns: HashMap<String, Rc<FnDecl>>,
    struct_fields: HashMap<String, Vec<String>>,
    env: Vec<HashMap<String, Value>>,
    depth: usize,
    /// When set, `print` appends here instead of writing to stdout.
    pub captured: Option<String>,
}

impl<'a> Interp<'a> {
    pub fn new(program: &Program, src: &'a str) -> Interp<'a> {
        let mut fns = HashMap::new();
        let mut struct_fields = HashMap::new();
        for item in &program.items {
            match item {
                Item::Fn(f) => {
                    fns.insert(f.name.clone(), f.clone());
                }
                Item::Struct(s) => {
                    struct_fields.insert(s.name.clone(), s.fields.iter().map(|f| f.name.clone()).collect());
                }
                _ => {}
            }
        }
        Interp { src, fns, struct_fields, env: Vec::new(), depth: 0, captured: None }
    }

    pub fn run_main(&mut self) -> Result<(), Diagnostic> {
        let Some(main) = self.fns.get("main").cloned() else {
            return Err(Diagnostic::new("E0207", "no `main` function found", Span::new(0, 0))
                .hint("add `fn main() uses io { ... }`"));
        };
        self.env = vec![HashMap::new()];
        self.depth = 0;
        match self.call_fn(&main, Vec::new(), main.name_span) {
            Ok(_) | Err(Ctrl::Return(_)) | Err(Ctrl::Break) | Err(Ctrl::Continue) => Ok(()),
            Err(Ctrl::Panic(d)) => Err(*d),
        }
    }

    pub fn run_test(&mut self, test: &TestDecl) -> Result<(), Diagnostic> {
        self.env = vec![HashMap::new()];
        self.depth = 0;
        match self.block(&test.body) {
            Ok(_) | Err(Ctrl::Return(_)) | Err(Ctrl::Break) | Err(Ctrl::Continue) => Ok(()),
            Err(Ctrl::Panic(d)) => Err(*d),
        }
    }

    fn text(&self, span: Span) -> &str {
        self.src.get(span.start..span.end).unwrap_or("")
    }

    // ---- calls ----

    fn call_fn(&mut self, f: &Rc<FnDecl>, args: Vec<Value>, span: Span) -> R<Value> {
        if self.depth >= MAX_DEPTH {
            return Err(panic_hint(
                "E0506",
                format!("stack overflow: more than {MAX_DEPTH} nested calls"),
                span,
                "check that the recursion has a base case",
            ));
        }
        let mut scope = HashMap::new();
        for (p, a) in f.params.iter().zip(args) {
            scope.insert(p.name.clone(), a);
        }
        let saved = std::mem::replace(&mut self.env, vec![scope]);
        self.depth += 1;
        let result = self.call_body(f, span);
        self.depth -= 1;
        self.env = saved;
        result
    }

    fn call_body(&mut self, f: &FnDecl, span: Span) -> R<Value> {
        for r in &f.requires {
            if self.eval(r)? != Value::Bool(true) {
                let msg = format!("precondition of `{}` violated: `{}`", f.name, self.text(r.span));
                return Err(panic("E0502", msg, span));
            }
        }
        let value = match self.block(&f.body) {
            Ok(v) => v,
            Err(Ctrl::Return(v)) => v,
            Err(other) => return Err(other),
        };
        let value = if f.ret.is_none() { Value::Unit } else { value };
        if !f.ensures.is_empty() {
            let mut scope = HashMap::new();
            scope.insert("result".to_string(), value.clone());
            self.env.push(scope);
            for e in &f.ensures {
                if self.eval(e)? != Value::Bool(true) {
                    let msg = format!(
                        "postcondition of `{}` violated: `{}` (result: {})",
                        f.name,
                        self.text(e.span),
                        value.repr()
                    );
                    return Err(panic("E0502", msg, span));
                }
            }
            self.env.pop();
        }
        Ok(value)
    }

    // ---- statements ----

    fn block(&mut self, block: &Block) -> R<Value> {
        self.env.push(HashMap::new());
        let result = self.stmts(block);
        self.env.pop();
        result
    }

    fn stmts(&mut self, block: &Block) -> R<Value> {
        let mut value = Value::Unit;
        for stmt in &block.stmts {
            value = match stmt {
                Stmt::Expr(e) => self.eval(e)?,
                other => {
                    self.stmt(other)?;
                    Value::Unit
                }
            };
        }
        Ok(value)
    }

    fn define(&mut self, name: &str, value: Value) {
        if name != "_" {
            self.env.last_mut().expect("scope").insert(name.to_string(), value);
        }
    }

    /// Runs a loop body. Returns false when the loop should stop.
    fn loop_body(&mut self, body: &Block) -> R<bool> {
        match self.block(body) {
            Ok(_) | Err(Ctrl::Continue) => Ok(true),
            Err(Ctrl::Break) => Ok(false),
            Err(other) => Err(other),
        }
    }

    fn stmt(&mut self, stmt: &Stmt) -> R<()> {
        match stmt {
            Stmt::Let { name, value, .. } => {
                let v = self.eval(value)?;
                self.define(name, v);
            }
            Stmt::Assign { target, value } => {
                let v = self.eval(value)?;
                *self.place(target)? = v;
            }
            Stmt::While { cond, body } => {
                while self.eval(cond)? == Value::Bool(true) {
                    if !self.loop_body(body)? {
                        break;
                    }
                }
            }
            Stmt::For { var, iter, end, body, .. } => {
                let start = self.eval(iter)?;
                let items: Vec<Value> = match end {
                    Some(end) => match (start, self.eval(end)?) {
                        (Value::Int(a), Value::Int(b)) => {
                            for i in a..b {
                                self.env.push(HashMap::new());
                                self.define(var, Value::Int(i));
                                let keep_going = self.loop_body(body);
                                self.env.pop();
                                if !keep_going? {
                                    break;
                                }
                            }
                            return Ok(());
                        }
                        _ => return Err(internal(iter.span)),
                    },
                    None => match start {
                        Value::List(items) => items.to_vec(),
                        _ => return Err(internal(iter.span)),
                    },
                };
                for item in items {
                    self.env.push(HashMap::new());
                    self.define(var, item);
                    let keep_going = self.loop_body(body);
                    self.env.pop();
                    if !keep_going? {
                        break;
                    }
                }
            }
            Stmt::Assert { cond, span } => {
                if let ExprKind::Binary(op, lhs, rhs) = &cond.kind {
                    if op.is_comparison() {
                        let l = self.eval(lhs)?;
                        let r = self.eval(rhs)?;
                        if self.compare(*op, &l, &r) {
                            return Ok(());
                        }
                        let msg = format!(
                            "assertion failed: `{}` (left: {}, right: {})",
                            self.text(cond.span),
                            l.repr(),
                            r.repr()
                        );
                        return Err(panic("E0501", msg, *span));
                    }
                }
                if self.eval(cond)? != Value::Bool(true) {
                    let msg = format!("assertion failed: `{}`", self.text(cond.span));
                    return Err(panic("E0501", msg, *span));
                }
            }
            Stmt::Expr(e) => {
                self.eval(e)?;
            }
        }
        Ok(())
    }

    /// Mutable access to an assignment target.
    fn place(&mut self, target: &Expr) -> R<&mut Value> {
        match &target.kind {
            ExprKind::Ident(name) => {
                for scope in self.env.iter_mut().rev() {
                    if let Some(v) = scope.get_mut(name) {
                        return Ok(v);
                    }
                }
                Err(internal(target.span))
            }
            ExprKind::Field(base, name, _) => match self.place(base)? {
                Value::Struct(s) => Rc::make_mut(s)
                    .fields
                    .iter_mut()
                    .find(|(n, _)| n == name)
                    .map(|(_, v)| v)
                    .ok_or_else(|| internal(target.span)),
                _ => Err(internal(target.span)),
            },
            ExprKind::Index(base, index) => {
                let index = self.eval(index)?;
                match (self.place(base)?, index) {
                    (Value::List(items), Value::Int(i)) => {
                        let len = items.len();
                        if i < 0 || i as usize >= len {
                            return Err(out_of_bounds(i, len, target.span));
                        }
                        Ok(&mut Rc::make_mut(items)[i as usize])
                    }
                    _ => Err(internal(target.span)),
                }
            }
            _ => Err(internal(target.span)),
        }
    }

    // ---- expressions ----

    fn lookup(&self, name: &str) -> Option<&Value> {
        self.env.iter().rev().find_map(|s| s.get(name))
    }

    fn eval_args(&mut self, args: &[Expr]) -> R<Vec<Value>> {
        let mut out = Vec::with_capacity(args.len());
        for a in args {
            out.push(self.eval(a)?);
        }
        Ok(out)
    }

    fn eval(&mut self, e: &Expr) -> R<Value> {
        match &e.kind {
            ExprKind::Int(n) => Ok(Value::Int(*n)),
            ExprKind::Float(f) => Ok(Value::Float(*f)),
            ExprKind::Bool(b) => Ok(Value::Bool(*b)),
            ExprKind::Str(parts) => {
                let mut out = String::new();
                for part in parts {
                    match part {
                        StrPart::Lit(s) => out.push_str(s),
                        StrPart::Expr(inner) => out.push_str(&self.eval(inner)?.display()),
                    }
                }
                Ok(Value::str(out))
            }
            ExprKind::Ident(name) => self.lookup(name).cloned().ok_or_else(|| internal(e.span)),
            ExprKind::List(items) => Ok(Value::List(Rc::new(self.eval_args(items)?))),
            ExprKind::Unary(op, operand) => {
                let v = self.eval(operand)?;
                match (op, v) {
                    (UnOp::Not, Value::Bool(b)) => Ok(Value::Bool(!b)),
                    (UnOp::Neg, Value::Float(f)) => Ok(Value::Float(-f)),
                    (UnOp::Neg, Value::Int(n)) => n
                        .checked_neg()
                        .map(Value::Int)
                        .ok_or_else(|| panic("E0503", "integer overflow in negation", e.span)),
                    _ => Err(internal(e.span)),
                }
            }
            ExprKind::Binary(op, lhs, rhs) => {
                let l = self.eval(lhs)?;
                match (op, &l) {
                    (BinOp::And, Value::Bool(false)) => return Ok(Value::Bool(false)),
                    (BinOp::Or, Value::Bool(true)) => return Ok(Value::Bool(true)),
                    (BinOp::And | BinOp::Or, _) => return self.eval(rhs),
                    _ => {}
                }
                let r = self.eval(rhs)?;
                if op.is_comparison() {
                    return Ok(Value::Bool(self.compare(*op, &l, &r)));
                }
                self.arith(*op, l, r, e.span)
            }
            ExprKind::Call { name, args, .. } => {
                let mut args = self.eval_args(args)?;
                match name.as_str() {
                    "print" => {
                        let text = args.pop().map(|v| v.display()).unwrap_or_default();
                        match &mut self.captured {
                            Some(buf) => {
                                buf.push_str(&text);
                                buf.push('\n');
                            }
                            None => println!("{text}"),
                        }
                        Ok(Value::Unit)
                    }
                    "read_line" => {
                        let mut line = String::new();
                        let _ = std::io::stdin().read_line(&mut line);
                        Ok(Value::str(line.trim_end_matches(['\n', '\r'])))
                    }
                    "panic" => {
                        let text = args.pop().map(|v| v.display()).unwrap_or_default();
                        Err(panic("E0500", format!("panic: {text}"), e.span))
                    }
                    _ => match self.fns.get(name).cloned() {
                        Some(f) => self.call_fn(&f, args, e.span),
                        None => Err(internal(e.span)),
                    },
                }
            }
            ExprKind::MethodCall { recv, name, args, .. } => {
                let recv = self.eval(recv)?;
                let args = self.eval_args(args)?;
                self.method(recv, name, args, e.span)
            }
            ExprKind::Field(base, name, _) => match self.eval(base)? {
                Value::Struct(s) => s
                    .fields
                    .iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, v)| v.clone())
                    .ok_or_else(|| internal(e.span)),
                _ => Err(internal(e.span)),
            },
            ExprKind::Index(base, index) => match (self.eval(base)?, self.eval(index)?) {
                (Value::List(items), Value::Int(i)) => {
                    if i < 0 || i as usize >= items.len() {
                        return Err(out_of_bounds(i, items.len(), e.span));
                    }
                    Ok(items[i as usize].clone())
                }
                _ => Err(internal(e.span)),
            },
            ExprKind::StructLit { name, fields } => {
                let mut given = Vec::with_capacity(fields.len());
                for (fname, _, value) in fields {
                    given.push((fname.clone(), self.eval(value)?));
                }
                // Store fields in declaration order so equality and display are stable.
                let order = self.struct_fields.get(name).cloned().unwrap_or_default();
                let mut ordered = Vec::with_capacity(given.len());
                for fname in &order {
                    match given.iter().position(|(n, _)| n == fname) {
                        Some(i) => ordered.push(given.swap_remove(i)),
                        None => return Err(internal(e.span)),
                    }
                }
                Ok(Value::Struct(Rc::new(StructVal { name: name.clone(), fields: ordered })))
            }
            ExprKind::Ctor { enum_name, variant, args } => {
                let args = self.eval_args(args)?;
                Ok(Value::variant(enum_name, variant, args))
            }
            ExprKind::If { cond, then, els } => {
                if self.eval(cond)? == Value::Bool(true) {
                    self.block(then)
                } else {
                    match els {
                        Some(els) => self.eval(els),
                        None => Ok(Value::Unit),
                    }
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                let value = self.eval(scrutinee)?;
                for arm in arms {
                    let mut scope = HashMap::new();
                    if matches(&arm.pat, &value, &mut scope) {
                        self.env.push(scope);
                        let result = self.eval(&arm.body);
                        self.env.pop();
                        return result;
                    }
                }
                Err(internal(e.span))
            }
            ExprKind::Block(b) => self.block(b),
            ExprKind::Return(value) => {
                let v = match value {
                    Some(v) => self.eval(v)?,
                    None => Value::Unit,
                };
                Err(Ctrl::Return(v))
            }
            ExprKind::Break => Err(Ctrl::Break),
            ExprKind::Continue => Err(Ctrl::Continue),
            ExprKind::Try(inner) => {
                let v = self.eval(inner)?;
                match &v {
                    Value::Enum(en) if en.variant == "Some" || en.variant == "Ok" => Ok(en.args[0].clone()),
                    Value::Enum(_) => Err(Ctrl::Return(v)),
                    _ => Err(internal(e.span)),
                }
            }
        }
    }

    fn compare(&self, op: BinOp, l: &Value, r: &Value) -> bool {
        match op {
            BinOp::Eq => l == r,
            BinOp::Ne => l != r,
            _ => {
                let ord = match (l, r) {
                    (Value::Int(a), Value::Int(b)) => Some(a.cmp(b)),
                    (Value::Float(a), Value::Float(b)) => a.partial_cmp(b),
                    (Value::Str(a), Value::Str(b)) => Some(a.cmp(b)),
                    _ => None,
                };
                match (op, ord) {
                    (BinOp::Lt, Some(o)) => o == Ordering::Less,
                    (BinOp::Le, Some(o)) => o != Ordering::Greater,
                    (BinOp::Gt, Some(o)) => o == Ordering::Greater,
                    (BinOp::Ge, Some(o)) => o != Ordering::Less,
                    _ => false,
                }
            }
        }
    }

    fn arith(&self, op: BinOp, l: Value, r: Value, span: Span) -> R<Value> {
        match (l, r) {
            (Value::Int(a), Value::Int(b)) => {
                if b == 0 && matches!(op, BinOp::Div | BinOp::Rem) {
                    return Err(panic_hint(
                        "E0504",
                        "division by zero",
                        span,
                        "check the divisor first, or add a `requires` clause",
                    ));
                }
                let result = match op {
                    BinOp::Add => a.checked_add(b),
                    BinOp::Sub => a.checked_sub(b),
                    BinOp::Mul => a.checked_mul(b),
                    BinOp::Div => a.checked_div(b),
                    BinOp::Rem => a.checked_rem(b),
                    _ => return Err(internal(span)),
                };
                result
                    .map(Value::Int)
                    .ok_or_else(|| panic("E0503", format!("integer overflow in `{a} {} {b}`", op.symbol()), span))
            }
            (Value::Float(a), Value::Float(b)) => Ok(Value::Float(match op {
                BinOp::Add => a + b,
                BinOp::Sub => a - b,
                BinOp::Mul => a * b,
                BinOp::Div => a / b,
                BinOp::Rem => a % b,
                _ => return Err(internal(span)),
            })),
            _ => Err(internal(span)),
        }
    }

    fn method(&self, recv: Value, name: &str, mut args: Vec<Value>, span: Span) -> R<Value> {
        if name == "to_string" {
            return Ok(Value::str(recv.display()));
        }
        let arg = args.pop();
        match (&recv, name, arg) {
            (Value::Str(s), "len", None) => Ok(Value::Int(s.chars().count() as i64)),
            (Value::Str(s), "contains", Some(Value::Str(needle))) => Ok(Value::Bool(s.contains(&*needle))),
            (Value::Str(s), "trim", None) => Ok(Value::str(s.trim())),
            (Value::Str(s), "to_int", None) => Ok(match s.trim().parse::<i64>() {
                Ok(n) => Value::some(Value::Int(n)),
                Err(_) => Value::none(),
            }),
            (Value::Str(s), "to_float", None) => Ok(match s.trim().parse::<f64>() {
                Ok(f) => Value::some(Value::Float(f)),
                Err(_) => Value::none(),
            }),
            (Value::List(items), "len", None) => Ok(Value::Int(items.len() as i64)),
            (Value::List(items), "push", Some(v)) => {
                let mut items = items.to_vec();
                items.push(v);
                Ok(Value::List(Rc::new(items)))
            }
            (Value::List(items), "get", Some(Value::Int(i))) => {
                Ok(match usize::try_from(i).ok().and_then(|i| items.get(i)) {
                    Some(v) => Value::some(v.clone()),
                    None => Value::none(),
                })
            }
            (Value::List(items), "contains", Some(v)) => Ok(Value::Bool(items.contains(&v))),
            (Value::Int(n), "to_float", None) => Ok(Value::Float(*n as f64)),
            (Value::Float(f), "to_int", None) => {
                if !f.is_finite() || *f >= 9.3e18 || *f <= -9.3e18 {
                    return Err(panic("E0503", format!("`{f}` cannot be converted to Int"), span));
                }
                Ok(Value::Int(f.trunc() as i64))
            }
            (Value::Enum(e), "unwrap_or", Some(default)) => Ok(match e.variant.as_str() {
                "Some" | "Ok" => e.args[0].clone(),
                _ => default,
            }),
            (Value::Enum(e), "is_some", None) => Ok(Value::Bool(e.variant == "Some")),
            (Value::Enum(e), "is_none", None) => Ok(Value::Bool(e.variant == "None")),
            (Value::Enum(e), "is_ok", None) => Ok(Value::Bool(e.variant == "Ok")),
            (Value::Enum(e), "is_err", None) => Ok(Value::Bool(e.variant == "Err")),
            _ => Err(internal(span)),
        }
    }
}

fn matches(pat: &Pattern, value: &Value, binds: &mut HashMap<String, Value>) -> bool {
    match (&pat.kind, value) {
        (PatKind::Wildcard, _) => true,
        (PatKind::Bind(name), v) => {
            binds.insert(name.clone(), v.clone());
            true
        }
        (PatKind::Int(a), Value::Int(b)) => a == b,
        (PatKind::Bool(a), Value::Bool(b)) => a == b,
        (PatKind::Str(a), Value::Str(b)) => a.as_str() == &**b,
        (PatKind::Ctor { variant, args, .. }, Value::Enum(e)) => {
            *variant == e.variant
                && args.len() == e.args.len()
                && args.iter().zip(&e.args).all(|(p, v)| matches(p, v, binds))
        }
        _ => false,
    }
}
