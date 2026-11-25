//! Tree-walking interpreter. Runs programs that have passed the type checker.

use crate::ast::*;
use crate::diag::{Diagnostic, Span};
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

/// Maximum depth of nested function calls.
pub const MAX_DEPTH: usize = 10_000;

#[derive(Clone)]
pub enum Value {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(Rc<str>),
    Unit,
    List(Rc<Vec<Value>>),
    Map(Rc<BTreeMap<Key, Value>>),
    Struct(Rc<StructVal>),
    Enum(Rc<EnumVal>),
    Fn(Rc<FnDecl>),
    Closure(Rc<ClosureVal>),
}

/// Map keys are restricted to types with a simple total order.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Key {
    Bool(bool),
    Int(i64),
    Str(Rc<str>),
}

#[derive(Clone, PartialEq)]
pub struct StructVal {
    pub name: String,
    pub fields: Vec<(String, Value)>,
}

#[derive(Clone, PartialEq)]
pub struct EnumVal {
    pub enum_name: String,
    pub variant: String,
    pub args: Vec<Value>,
}

pub struct ClosureVal {
    params: Vec<String>,
    body: Rc<Expr>,
    captured: HashMap<String, Value>,
}

impl std::fmt::Debug for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.repr())
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a == b,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Str(a), Value::Str(b)) => a == b,
            (Value::Unit, Value::Unit) => true,
            (Value::List(a), Value::List(b)) => a == b,
            (Value::Map(a), Value::Map(b)) => a == b,
            (Value::Struct(a), Value::Struct(b)) => a == b,
            (Value::Enum(a), Value::Enum(b)) => a == b,
            (Value::Fn(a), Value::Fn(b)) => Rc::ptr_eq(a, b),
            (Value::Closure(a), Value::Closure(b)) => Rc::ptr_eq(a, b),
            _ => false,
        }
    }
}

fn short(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

impl Key {
    fn to_value(&self) -> Value {
        match self {
            Key::Bool(b) => Value::Bool(*b),
            Key::Int(n) => Value::Int(*n),
            Key::Str(s) => Value::Str(s.clone()),
        }
    }
}

impl Value {
    fn str(s: impl AsRef<str>) -> Value {
        Value::Str(Rc::from(s.as_ref()))
    }

    fn list(items: Vec<Value>) -> Value {
        Value::List(Rc::new(items))
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

    fn option(v: Option<Value>) -> Value {
        match v {
            Some(v) => Value::some(v),
            None => Value::none(),
        }
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
        let join = |items: &[Value]| items.iter().map(|v| v.repr()).collect::<Vec<_>>().join(", ");
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
            Value::List(items) => format!("[{}]", join(items)),
            Value::Map(entries) => {
                if entries.is_empty() {
                    return "Map {}".to_string();
                }
                let parts: Vec<String> =
                    entries.iter().map(|(k, v)| format!("{}: {}", k.to_value().repr(), v.repr())).collect();
                format!("Map {{ {} }}", parts.join(", "))
            }
            Value::Struct(s) => {
                let parts: Vec<String> = s.fields.iter().map(|(n, v)| format!("{n}: {}", v.repr())).collect();
                format!("{} {{ {} }}", short(&s.name), parts.join(", "))
            }
            Value::Enum(e) => {
                let mut out = match e.enum_name.as_str() {
                    "Option" | "Result" => e.variant.clone(),
                    name => format!("{}.{}", short(name), e.variant),
                };
                if !e.args.is_empty() {
                    out.push_str(&format!("({})", join(&e.args)));
                }
                out
            }
            Value::Fn(f) => format!("<fn {}>", short(&f.name)),
            Value::Closure(_) => "<closure>".to_string(),
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

fn missing_key(key: &Key, span: Span) -> Ctrl {
    panic_hint(
        "E0507",
        format!("key {} is not in the map", key.to_value().repr()),
        span,
        "use `.get(key)` to receive an Option, or check with `.has(key)`",
    )
}

fn to_key(v: &Value, span: Span) -> R<Key> {
    match v {
        Value::Bool(b) => Ok(Key::Bool(*b)),
        Value::Int(n) => Ok(Key::Int(*n)),
        Value::Str(s) => Ok(Key::Str(s.clone())),
        other => Err(panic_hint(
            "E0508",
            format!("{} cannot be used as a Map key", other.repr()),
            span,
            "Map keys must be Int, String or Bool",
        )),
    }
}

/// Checks `start..end` against a length and returns it as a usize range.
fn slice_range(start: &Value, end: &Value, len: usize, span: Span) -> R<(usize, usize)> {
    match (start, end) {
        (Value::Int(a), Value::Int(b)) if 0 <= *a && a <= b && *b as usize <= len => Ok((*a as usize, *b as usize)),
        (Value::Int(a), Value::Int(b)) => Err(panic(
            "E0505",
            format!("slice {a}..{b} is out of bounds for a length of {len}"),
            span,
        )),
        _ => Err(internal(span)),
    }
}

fn order(a: &Value, b: &Value) -> Option<Ordering> {
    match (a, b) {
        (Value::Int(a), Value::Int(b)) => Some(a.cmp(b)),
        (Value::Float(a), Value::Float(b)) => a.partial_cmp(b),
        (Value::Str(a), Value::Str(b)) => Some(a.cmp(b)),
        _ => None,
    }
}

pub struct Interp<'a> {
    src: &'a str,
    fns: HashMap<String, Rc<FnDecl>>,
    struct_fields: HashMap<String, Vec<String>>,
    env: Vec<HashMap<String, Value>>,
    depth: usize,
    /// Command line arguments returned by `args()`.
    pub args: Vec<String>,
    /// When set, `print` appends here instead of writing to stdout.
    pub captured: Option<String>,
}

impl<'a> Interp<'a> {
    /// `src` is the text of the source map the program was parsed from.
    pub fn new(program: &Program, src: &'a str) -> Interp<'a> {
        let mut fns = HashMap::new();
        let mut struct_fields = HashMap::new();
        for item in &program.items {
            match item {
                Item::Fn(f) => {
                    fns.entry(f.name.clone()).or_insert_with(|| f.clone());
                }
                Item::Struct(s) => {
                    struct_fields.insert(s.name.clone(), s.fields.iter().map(|f| f.name.clone()).collect());
                }
                _ => {}
            }
        }
        Interp { src, fns, struct_fields, env: Vec::new(), depth: 0, args: Vec::new(), captured: None }
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

    fn enter(&mut self, span: Span) -> R<()> {
        if self.depth >= MAX_DEPTH {
            return Err(panic_hint(
                "E0506",
                format!("stack overflow: more than {MAX_DEPTH} nested calls"),
                span,
                "check that the recursion has a base case",
            ));
        }
        Ok(())
    }

    fn call_fn(&mut self, f: &Rc<FnDecl>, args: Vec<Value>, span: Span) -> R<Value> {
        self.enter(span)?;
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
                let msg = format!("precondition of `{}` violated: `{}`", short(&f.name), self.text(r.span));
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
                        short(&f.name),
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

    /// Calls a function value (a named function or a closure).
    fn call_value(&mut self, f: &Value, args: Vec<Value>, span: Span) -> R<Value> {
        match f {
            Value::Fn(decl) => self.call_fn(decl, args, span),
            Value::Closure(c) => {
                self.enter(span)?;
                let mut scope = c.captured.clone();
                for (p, a) in c.params.iter().zip(args) {
                    if p != "_" {
                        scope.insert(p.clone(), a);
                    }
                }
                let saved = std::mem::replace(&mut self.env, vec![scope]);
                self.depth += 1;
                let result = match self.eval(&c.body) {
                    Ok(v) | Err(Ctrl::Return(v)) => Ok(v),
                    Err(other) => Err(other),
                };
                self.depth -= 1;
                self.env = saved;
                result
            }
            _ => Err(internal(span)),
        }
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

    /// Runs one iteration of a loop. Returns false when the loop should stop.
    fn iteration(&mut self, var: Option<(&str, Value)>, body: &Block) -> R<bool> {
        self.env.push(HashMap::new());
        if let Some((name, value)) = var {
            self.define(name, value);
        }
        let result = self.block(body);
        self.env.pop();
        match result {
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
                // Assigning to a map entry may insert a new key.
                if let ExprKind::Index(base, index) = &target.kind {
                    let index = self.eval(index)?;
                    if let Value::Map(entries) = self.place(base)? {
                        let key = to_key(&index, target.span)?;
                        Rc::make_mut(entries).insert(key, v);
                        return Ok(());
                    }
                    *self.element(base, index, target.span)? = v;
                    return Ok(());
                }
                *self.place(target)? = v;
            }
            Stmt::While { cond, body, .. } => {
                while self.eval(cond)? == Value::Bool(true) {
                    if !self.iteration(None, body)? {
                        break;
                    }
                }
            }
            Stmt::For { var, iter, end, body, .. } => {
                let start = self.eval(iter)?;
                match end {
                    Some(end) => match (start, self.eval(end)?) {
                        (Value::Int(a), Value::Int(b)) => {
                            for i in a..b {
                                if !self.iteration(Some((var, Value::Int(i))), body)? {
                                    break;
                                }
                            }
                        }
                        _ => return Err(internal(iter.span)),
                    },
                    None => match start {
                        Value::List(items) => {
                            for item in items.iter() {
                                if !self.iteration(Some((var, item.clone())), body)? {
                                    break;
                                }
                            }
                        }
                        _ => return Err(internal(iter.span)),
                    },
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

    /// Mutable access to `base[index]` for an existing element.
    fn element(&mut self, base: &Expr, index: Value, span: Span) -> R<&mut Value> {
        match (self.place(base)?, index) {
            (Value::List(items), Value::Int(i)) => {
                let len = items.len();
                if i < 0 || i as usize >= len {
                    return Err(out_of_bounds(i, len, span));
                }
                Ok(&mut Rc::make_mut(items)[i as usize])
            }
            (Value::Map(entries), index) => {
                let key = to_key(&index, span)?;
                match Rc::make_mut(entries).get_mut(&key) {
                    Some(v) => Ok(v),
                    None => Err(missing_key(&key, span)),
                }
            }
            _ => Err(internal(span)),
        }
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
                self.element(base, index, target.span)
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
            ExprKind::Unit => Ok(Value::Unit),
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
            ExprKind::FnRef(name) => self.fns.get(name).cloned().map(Value::Fn).ok_or_else(|| internal(e.span)),
            ExprKind::List(items) => Ok(Value::list(self.eval_args(items)?)),
            ExprKind::MapLit(entries) => {
                let mut map = BTreeMap::new();
                for (k, v) in entries {
                    let key = self.eval(k)?;
                    let key = to_key(&key, k.span)?;
                    map.insert(key, self.eval(v)?);
                }
                Ok(Value::Map(Rc::new(map)))
            }
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
            ExprKind::Call { name, local, args, .. } => {
                let args = self.eval_args(args)?;
                if *local {
                    let f = self.lookup(name).cloned().ok_or_else(|| internal(e.span))?;
                    return self.call_value(&f, args, e.span);
                }
                match self.fns.get(name).cloned() {
                    Some(f) => self.call_fn(&f, args, e.span),
                    None => self.builtin(name, args, e.span),
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
                (Value::Map(entries), index) => {
                    let key = to_key(&index, e.span)?;
                    entries.get(&key).cloned().ok_or_else(|| missing_key(&key, e.span))
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
            ExprKind::Closure { params, captures, body } => {
                let mut captured = HashMap::new();
                for name in captures {
                    if let Some(v) = self.lookup(name) {
                        captured.insert(name.clone(), v.clone());
                    }
                }
                Ok(Value::Closure(Rc::new(ClosureVal {
                    params: params.iter().map(|p| p.name.clone()).collect(),
                    body: body.clone(),
                    captured,
                })))
            }
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

    fn builtin(&mut self, name: &str, mut args: Vec<Value>, span: Span) -> R<Value> {
        let text = |v: Option<Value>| v.map(|v| v.display()).unwrap_or_default();
        let result = |r: Result<Value, String>| match r {
            Ok(v) => Value::variant("Result", "Ok", vec![v]),
            Err(e) => Value::variant("Result", "Err", vec![Value::str(e)]),
        };
        match name {
            "print" => {
                let text = text(args.pop());
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
            "args" => Ok(Value::list(self.args.iter().map(Value::str).collect())),
            "panic" => Err(panic("E0500", format!("panic: {}", text(args.pop())), span)),
            "read_file" => {
                let path = text(args.pop());
                Ok(result(
                    std::fs::read_to_string(&path).map(Value::str).map_err(|e| format!("cannot read `{path}`: {e}")),
                ))
            }
            "write_file" => {
                let content = text(args.pop());
                let path = text(args.pop());
                Ok(result(
                    std::fs::write(&path, content)
                        .map(|_| Value::Unit)
                        .map_err(|e| format!("cannot write `{path}`: {e}")),
                ))
            }
            _ => Err(internal(span)),
        }
    }

    fn compare(&self, op: BinOp, l: &Value, r: &Value) -> bool {
        match op {
            BinOp::Eq => l == r,
            BinOp::Ne => l != r,
            _ => match (op, order(l, r)) {
                (BinOp::Lt, Some(o)) => o == Ordering::Less,
                (BinOp::Le, Some(o)) => o != Ordering::Greater,
                (BinOp::Gt, Some(o)) => o == Ordering::Greater,
                (BinOp::Ge, Some(o)) => o != Ordering::Less,
                _ => false,
            },
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

    fn method(&mut self, recv: Value, name: &str, args: Vec<Value>, span: Span) -> R<Value> {
        if name == "to_string" {
            return Ok(Value::str(recv.display()));
        }
        let truthy = |v: Value| v == Value::Bool(true);
        match (&recv, name, args.as_slice()) {
            (Value::Str(s), "len", []) => Ok(Value::Int(s.chars().count() as i64)),
            (Value::Str(s), "is_empty", []) => Ok(Value::Bool(s.is_empty())),
            (Value::Str(s), "contains", [Value::Str(x)]) => Ok(Value::Bool(s.contains(&**x))),
            (Value::Str(s), "starts_with", [Value::Str(x)]) => Ok(Value::Bool(s.starts_with(&**x))),
            (Value::Str(s), "ends_with", [Value::Str(x)]) => Ok(Value::Bool(s.ends_with(&**x))),
            (Value::Str(s), "trim", []) => Ok(Value::str(s.trim())),
            (Value::Str(s), "to_upper", []) => Ok(Value::str(s.to_uppercase())),
            (Value::Str(s), "to_lower", []) => Ok(Value::str(s.to_lowercase())),
            (Value::Str(s), "replace", [Value::Str(from), Value::Str(to)]) => {
                if from.is_empty() {
                    return Ok(recv.clone());
                }
                Ok(Value::str(s.replace(&**from, to)))
            }
            (Value::Str(s), "split", [Value::Str(sep)]) => {
                let parts: Vec<Value> = if sep.is_empty() {
                    s.chars().map(|c| Value::str(c.to_string())).collect()
                } else {
                    s.split(&**sep).map(Value::str).collect()
                };
                Ok(Value::list(parts))
            }
            (Value::Str(s), "chars", []) => Ok(Value::list(s.chars().map(|c| Value::str(c.to_string())).collect())),
            (Value::Str(s), "slice", [start, end]) => {
                let chars: Vec<char> = s.chars().collect();
                let (a, b) = slice_range(start, end, chars.len(), span)?;
                Ok(Value::str(chars[a..b].iter().collect::<String>()))
            }
            (Value::Str(s), "to_int", []) => Ok(Value::option(s.trim().parse::<i64>().ok().map(Value::Int))),
            (Value::Str(s), "to_float", []) => Ok(Value::option(s.trim().parse::<f64>().ok().map(Value::Float))),

            (Value::List(items), "len", []) => Ok(Value::Int(items.len() as i64)),
            (Value::List(items), "is_empty", []) => Ok(Value::Bool(items.is_empty())),
            (Value::List(items), "push", [v]) => {
                let mut items = items.to_vec();
                items.push(v.clone());
                Ok(Value::list(items))
            }
            (Value::List(items), "get", [Value::Int(i)]) => {
                Ok(Value::option(usize::try_from(*i).ok().and_then(|i| items.get(i)).cloned()))
            }
            (Value::List(items), "contains", [v]) => Ok(Value::Bool(items.contains(v))),
            (Value::List(items), "first", []) => Ok(Value::option(items.first().cloned())),
            (Value::List(items), "last", []) => Ok(Value::option(items.last().cloned())),
            (Value::List(items), "concat", [Value::List(other)]) => {
                Ok(Value::list(items.iter().chain(other.iter()).cloned().collect()))
            }
            (Value::List(items), "reverse", []) => Ok(Value::list(items.iter().rev().cloned().collect())),
            (Value::List(items), "slice", [start, end]) => {
                let (a, b) = slice_range(start, end, items.len(), span)?;
                Ok(Value::list(items[a..b].to_vec()))
            }
            (Value::List(items), "sort", []) => {
                let mut items = items.to_vec();
                items.sort_by(|a, b| order(a, b).unwrap_or(Ordering::Equal));
                Ok(Value::list(items))
            }
            (Value::List(items), "join", [Value::Str(sep)]) => {
                let parts: Vec<String> = items.iter().map(|v| v.display()).collect();
                Ok(Value::str(parts.join(sep)))
            }
            (Value::List(items), "map", [f]) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items.iter() {
                    out.push(self.call_value(f, vec![item.clone()], span)?);
                }
                Ok(Value::list(out))
            }
            (Value::List(items), "filter", [f]) => {
                let mut out = Vec::new();
                for item in items.iter() {
                    if truthy(self.call_value(f, vec![item.clone()], span)?) {
                        out.push(item.clone());
                    }
                }
                Ok(Value::list(out))
            }
            (Value::List(items), "any", [f]) => {
                for item in items.iter() {
                    if truthy(self.call_value(f, vec![item.clone()], span)?) {
                        return Ok(Value::Bool(true));
                    }
                }
                Ok(Value::Bool(false))
            }
            (Value::List(items), "all", [f]) => {
                for item in items.iter() {
                    if !truthy(self.call_value(f, vec![item.clone()], span)?) {
                        return Ok(Value::Bool(false));
                    }
                }
                Ok(Value::Bool(true))
            }
            (Value::List(items), "fold", [init, f]) => {
                let mut acc = init.clone();
                for item in items.iter() {
                    acc = self.call_value(f, vec![acc, item.clone()], span)?;
                }
                Ok(acc)
            }

            (Value::Map(entries), "len", []) => Ok(Value::Int(entries.len() as i64)),
            (Value::Map(entries), "is_empty", []) => Ok(Value::Bool(entries.is_empty())),
            (Value::Map(entries), "get", [k]) => Ok(Value::option(entries.get(&to_key(k, span)?).cloned())),
            (Value::Map(entries), "has", [k]) => Ok(Value::Bool(entries.contains_key(&to_key(k, span)?))),
            (Value::Map(entries), "set", [k, v]) => {
                let mut entries = (**entries).clone();
                entries.insert(to_key(k, span)?, v.clone());
                Ok(Value::Map(Rc::new(entries)))
            }
            (Value::Map(entries), "remove", [k]) => {
                let mut entries = (**entries).clone();
                entries.remove(&to_key(k, span)?);
                Ok(Value::Map(Rc::new(entries)))
            }
            (Value::Map(entries), "keys", []) => Ok(Value::list(entries.keys().map(|k| k.to_value()).collect())),
            (Value::Map(entries), "values", []) => Ok(Value::list(entries.values().cloned().collect())),

            (Value::Int(n), "to_float", []) => Ok(Value::Float(*n as f64)),
            (Value::Int(n), "abs", []) => {
                n.checked_abs().map(Value::Int).ok_or_else(|| panic("E0503", "integer overflow in `abs`", span))
            }
            (Value::Int(a), "min", [Value::Int(b)]) => Ok(Value::Int(*a.min(b))),
            (Value::Int(a), "max", [Value::Int(b)]) => Ok(Value::Int(*a.max(b))),
            (Value::Float(f), "to_int", []) => {
                if !f.is_finite() || *f >= 9.3e18 || *f <= -9.3e18 {
                    return Err(panic("E0503", format!("`{f}` cannot be converted to Int"), span));
                }
                Ok(Value::Int(f.trunc() as i64))
            }
            (Value::Float(f), "abs", []) => Ok(Value::Float(f.abs())),
            (Value::Float(f), "sqrt", []) => Ok(Value::Float(f.sqrt())),
            (Value::Float(f), "floor", []) => Ok(Value::Float(f.floor())),
            (Value::Float(f), "ceil", []) => Ok(Value::Float(f.ceil())),
            (Value::Float(f), "round", []) => Ok(Value::Float(f.round())),
            (Value::Float(a), "min", [Value::Float(b)]) => Ok(Value::Float(a.min(*b))),
            (Value::Float(a), "max", [Value::Float(b)]) => Ok(Value::Float(a.max(*b))),

            (Value::Enum(e), "unwrap_or", [default]) => Ok(match e.variant.as_str() {
                "Some" | "Ok" => e.args[0].clone(),
                _ => default.clone(),
            }),
            (Value::Enum(e), "is_some", []) => Ok(Value::Bool(e.variant == "Some")),
            (Value::Enum(e), "is_none", []) => Ok(Value::Bool(e.variant == "None")),
            (Value::Enum(e), "is_ok", []) => Ok(Value::Bool(e.variant == "Ok")),
            (Value::Enum(e), "is_err", []) => Ok(Value::Bool(e.variant == "Err")),
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
