//! Type checker: types, generics, effects, mutability, unused values and
//! match exhaustiveness.

use crate::ast::*;
use crate::diag::{Diagnostic, Span};
use std::collections::{HashMap, HashSet};

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
    Map(Box<Type>, Box<Type>),
    Struct(String, Vec<Type>),
    Enum(String, Vec<Type>),
    /// Parameters, return type and effects (sorted).
    Fn(Vec<Type>, Box<Type>, Vec<String>),
    /// A type parameter of the declaration being checked.
    Param(String),
    /// Inference variable.
    Var(usize),
}

/// Marker effect on the function parameters of built-in methods such as
/// `map`: any effects are accepted and charged to the caller.
const ANY_EFFECT: &str = "*";

#[derive(Clone)]
struct FnSig {
    type_params: Vec<String>,
    params: Vec<Type>,
    ret: Type,
    effects: Vec<String>,
}

#[derive(Clone, Default)]
struct StructInfo {
    params: Vec<String>,
    fields: Vec<(String, Type)>,
}

#[derive(Clone, Default)]
struct EnumInfo {
    params: Vec<String>,
    variants: Vec<(String, Vec<Type>)>,
}

/// Simplified pattern used by the exhaustiveness check.
#[derive(Clone)]
enum SPat {
    Wild,
    Con(String, Vec<SPat>),
}

struct Ctor {
    key: String,
    display: String,
    fields: Vec<Type>,
}

struct Checker {
    structs: HashMap<String, StructInfo>,
    enums: HashMap<String, EnumInfo>,
    fns: HashMap<String, FnSig>,
    subst: Vec<Option<Type>>,
    /// name -> (type, mutable)
    scopes: Vec<HashMap<String, (Type, bool)>>,
    tparams: Vec<String>,
    ret: Type,
    effects: Vec<String>,
    /// Set while checking a closure body: effects used so far.
    infer_effects: Option<Vec<String>>,
    /// Scopes below this index belong to an enclosing function (captured by value).
    closure_base: usize,
    in_contract: bool,
    loop_depth: usize,
    diags: Vec<Diagnostic>,
    /// Type of every declared variable, by the start of its declaration span.
    decl_types: Vec<(usize, Type)>,
    /// Operand type of every arithmetic and comparison expression, by span.
    op_types: Vec<((usize, usize), Type)>,
    /// Position of the field read by every field access, by span.
    field_uses: Vec<((usize, usize), u32)>,
}

/// Checks the program. The second result tells the code generator which
/// variables and operations are known to work on `Int`.
pub fn check(program: &Program) -> (Vec<Diagnostic>, TypeInfo) {
    let mut c = Checker {
        structs: HashMap::new(),
        enums: HashMap::new(),
        fns: HashMap::new(),
        subst: Vec::new(),
        scopes: Vec::new(),
        tparams: Vec::new(),
        ret: Type::Unit,
        effects: Vec::new(),
        infer_effects: None,
        closure_base: 0,
        in_contract: false,
        loop_depth: 0,
        diags: Vec::new(),
        decl_types: Vec::new(),
        op_types: Vec::new(),
        field_uses: Vec::new(),
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
    // Inference is complete, so every recorded type can now be resolved.
    let mut info = TypeInfo::default();
    for (at, ty) in &c.decl_types {
        if c.apply(ty) == Type::Int {
            info.int_vars.insert(*at);
        }
    }
    for (at, ty) in &c.op_types {
        if c.apply(ty) == Type::Int {
            info.int_ops.insert(*at);
        }
    }
    for (name, sig) in &c.fns {
        if sig.ret == Type::Int {
            info.int_fns.insert(name.clone());
        }
    }
    info.field_index = c.field_uses.iter().copied().collect();
    (c.diags, info)
}

/// Last segment of a module-qualified name.
fn short(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
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

fn substitute(t: &Type, map: &HashMap<String, Type>) -> Type {
    let all = |ts: &[Type]| ts.iter().map(|t| substitute(t, map)).collect::<Vec<_>>();
    let one = |t: &Type| Box::new(substitute(t, map));
    match t {
        Type::Param(name) => map.get(name).cloned().unwrap_or_else(|| t.clone()),
        Type::List(a) => Type::List(one(a)),
        Type::Option(a) => Type::Option(one(a)),
        Type::Result(a, b) => Type::Result(one(a), one(b)),
        Type::Map(a, b) => Type::Map(one(a), one(b)),
        Type::Struct(n, args) => Type::Struct(n.clone(), all(args)),
        Type::Enum(n, args) => Type::Enum(n.clone(), all(args)),
        Type::Fn(params, ret, effects) => Type::Fn(all(params), one(ret), effects.clone()),
        other => other.clone(),
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

    /// Fresh inference variables for a list of type parameters.
    fn instantiate(&mut self, params: &[String]) -> HashMap<String, Type> {
        params.iter().map(|p| (p.clone(), self.fresh())).collect()
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
        let all = |ts: &[Type]| ts.iter().map(|t| self.apply(t)).collect::<Vec<_>>();
        let one = |t: &Type| Box::new(self.apply(t));
        match self.resolve(t) {
            Type::List(a) => Type::List(one(&a)),
            Type::Option(a) => Type::Option(one(&a)),
            Type::Result(a, b) => Type::Result(one(&a), one(&b)),
            Type::Map(a, b) => Type::Map(one(&a), one(&b)),
            Type::Struct(n, args) => Type::Struct(n, all(&args)),
            Type::Enum(n, args) => Type::Enum(n, all(&args)),
            Type::Fn(params, ret, effects) => Type::Fn(all(&params), one(&ret), effects),
            other => other,
        }
    }

    fn show(&self, t: &Type) -> String {
        let list = |ts: &[Type]| ts.iter().map(|t| self.show(t)).collect::<Vec<_>>().join(", ");
        let named = |n: &str, args: &[Type]| {
            if args.is_empty() {
                n.to_string()
            } else {
                format!("{n}<{}>", list(args))
            }
        };
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
            Type::Map(a, b) => format!("Map<{}, {}>", self.show(&a), self.show(&b)),
            Type::Struct(n, args) | Type::Enum(n, args) => named(&n, &args),
            Type::Fn(params, ret, effects) => {
                let mut out = format!("fn({})", list(&params));
                if self.resolve(&ret) != Type::Unit {
                    out.push_str(&format!(" -> {}", self.show(&ret)));
                }
                let effects: Vec<&str> =
                    effects.iter().map(|e| e.as_str()).filter(|e| *e != ANY_EFFECT).collect();
                if !effects.is_empty() {
                    out.push_str(&format!(" uses {}", effects.join(", ")));
                }
                out
            }
            Type::Param(n) => n,
            Type::Var(_) => "_".to_string(),
        }
    }

    fn occurs(&self, v: usize, t: &Type) -> bool {
        match self.resolve(t) {
            Type::Var(w) => v == w,
            Type::List(a) | Type::Option(a) => self.occurs(v, &a),
            Type::Result(a, b) | Type::Map(a, b) => self.occurs(v, &a) || self.occurs(v, &b),
            Type::Struct(_, args) | Type::Enum(_, args) => args.iter().any(|a| self.occurs(v, a)),
            Type::Fn(params, ret, _) => params.iter().any(|p| self.occurs(v, p)) || self.occurs(v, &ret),
            _ => false,
        }
    }

    fn unify_all(&mut self, a: &[Type], b: &[Type]) -> bool {
        if a.len() != b.len() {
            return false;
        }
        let mut ok = true;
        for (x, y) in a.iter().zip(b) {
            ok &= self.unify(x, y);
        }
        ok
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
            (Type::Result(x1, x2), Type::Result(y1, y2)) | (Type::Map(x1, x2), Type::Map(y1, y2)) => {
                let first = self.unify(x1, y1);
                let second = self.unify(x2, y2);
                first && second
            }
            (Type::Struct(n1, a1), Type::Struct(n2, a2)) | (Type::Enum(n1, a1), Type::Enum(n2, a2)) => {
                n1 == n2 && self.unify_all(a1, a2)
            }
            (Type::Fn(p1, r1, e1), Type::Fn(p2, r2, e2)) => {
                let params = self.unify_all(p1, p2);
                let ret = self.unify(r1, r2);
                params && ret && e1 == e2
            }
            _ => a == b,
        }
    }

    fn expect(&mut self, actual: &Type, expected: &Type, span: Span) {
        // A function with fewer effects can be used where more are allowed.
        if let (Type::Fn(ap, ar, ae), Type::Fn(_, _, ee)) = (self.resolve(actual), self.resolve(expected)) {
            if ae != ee && ae.iter().all(|e| ee.contains(e)) {
                let widened = Type::Fn(ap, ar, ee);
                return self.expect(&widened, expected, span);
            }
        }
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
                (Type::Fn(_, _, ee), Type::Fn(_, _, ae)) if ee != ae => {
                    Some("the function uses effects that are not allowed here")
                }
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

    fn check_map_key(&mut self, key: &Type, span: Span) {
        match self.resolve(key) {
            Type::Int | Type::Str | Type::Bool | Type::Var(_) | Type::Param(_) | Type::Error | Type::Never => {}
            _ => {
                let msg = format!("`{}` cannot be used as a Map key", self.show(key));
                self.error_hint("E0315", msg, span, "Map keys must be Int, String or Bool");
            }
        }
    }

    fn check_effect_names(&mut self, effects: &[String], span: Span) -> Vec<String> {
        let mut out = Vec::new();
        for e in effects {
            if !EFFECTS.contains(&e.as_str()) {
                self.error_hint(
                    "E0402",
                    format!("unknown effect `{e}`"),
                    span,
                    format!("available effects: {}", EFFECTS.join(", ")),
                );
            } else if !out.contains(e) {
                out.push(e.clone());
            }
        }
        out.sort();
        out
    }

    fn resolve_type(&mut self, t: &TypeExpr) -> Type {
        let (name, targs) = match &t.kind {
            TypeKind::Fn(params, ret, effects) => {
                let params = params.iter().map(|p| self.resolve_type(p)).collect();
                let ret = match ret {
                    Some(r) => self.resolve_type(r),
                    None => Type::Unit,
                };
                let effects = self.check_effect_names(effects, t.span);
                return Type::Fn(params, Box::new(ret), effects);
            }
            TypeKind::Named(name, args) => (name.as_str(), args),
        };
        if self.tparams.iter().any(|p| p == name) {
            if !targs.is_empty() {
                self.error("E0206", format!("type parameter `{name}` takes no type arguments"), t.span);
            }
            return Type::Param(name.to_string());
        }
        let arity = match name {
            "Int" | "Float" | "Bool" | "String" | "Unit" => 0,
            "List" | "Option" => 1,
            "Result" | "Map" => 2,
            _ => match (self.structs.get(name), self.enums.get(name)) {
                (Some(s), _) => s.params.len(),
                (_, Some(e)) => e.params.len(),
                _ => {
                    let known: Vec<String> =
                        self.structs.keys().chain(self.enums.keys()).map(|k| short(k).to_string()).collect();
                    let mut d = Diagnostic::new("E0202", format!("unknown type `{name}`"), t.span);
                    if let Some(s) = closest(short(name), known.iter()) {
                        d = d.hint(format!("did you mean `{s}`?"));
                    }
                    self.diags.push(d);
                    return Type::Error;
                }
            },
        };
        if targs.len() != arity {
            self.error(
                "E0206",
                format!("type `{name}` takes {arity} type argument(s), found {}", targs.len()),
                t.span,
            );
            return Type::Error;
        }
        let mut args: Vec<Type> = targs.iter().map(|a| self.resolve_type(a)).collect();
        match name {
            "Int" => Type::Int,
            "Float" => Type::Float,
            "Bool" => Type::Bool,
            "String" => Type::Str,
            "Unit" => Type::Unit,
            "List" => Type::List(Box::new(args.remove(0))),
            "Option" => Type::Option(Box::new(args.remove(0))),
            "Result" | "Map" => {
                let first = args.remove(0);
                let second = args.remove(0);
                if name == "Map" {
                    self.check_map_key(&first, targs[0].span);
                    Type::Map(Box::new(first), Box::new(second))
                } else {
                    Type::Result(Box::new(first), Box::new(second))
                }
            }
            _ if self.structs.contains_key(name) => Type::Struct(name.to_string(), args),
            _ => Type::Enum(name.to_string(), args),
        }
    }

    // ---- declarations ----

    /// Registers all declarations. Returns one signature per `fn` item, in source order.
    fn collect(&mut self, program: &Program) -> Vec<FnSig> {
        // Items whose name was already taken; their bodies are still checked.
        let mut duplicates: HashSet<usize> = HashSet::new();
        for (i, item) in program.items.iter().enumerate() {
            let (name, span, params) = match item {
                Item::Struct(s) => (&s.name, s.name_span, &s.type_params),
                Item::Enum(e) => (&e.name, e.name_span, &e.type_params),
                _ => continue,
            };
            let reserved = BUILTIN_TYPES.contains(&short(name)) || ["Some", "None", "Ok", "Err"].contains(&short(name));
            if reserved {
                self.error("E0201", format!("`{}` is a built-in name and cannot be redefined", short(name)), span);
                duplicates.insert(i);
            } else if self.structs.contains_key(name) || self.enums.contains_key(name) {
                self.error("E0201", format!("type `{name}` is defined more than once"), span);
                duplicates.insert(i);
            } else if matches!(item, Item::Struct(_)) {
                self.structs.insert(name.clone(), StructInfo { params: params.clone(), fields: Vec::new() });
            } else {
                self.enums.insert(name.clone(), EnumInfo { params: params.clone(), variants: Vec::new() });
            }
        }

        for (i, item) in program.items.iter().enumerate() {
            match item {
                Item::Struct(s) => {
                    self.tparams = s.type_params.clone();
                    let mut fields: Vec<(String, Type)> = Vec::new();
                    for f in &s.fields {
                        let ty = self.resolve_type(&f.ty);
                        if fields.iter().any(|(n, _)| n == &f.name) {
                            self.error("E0201", format!("field `{}` is defined more than once", f.name), f.span);
                        } else {
                            fields.push((f.name.clone(), ty));
                        }
                    }
                    if !duplicates.contains(&i) {
                        if let Some(info) = self.structs.get_mut(&s.name) {
                            info.fields = fields;
                        }
                    }
                }
                Item::Enum(e) => {
                    self.tparams = e.type_params.clone();
                    let mut variants: Vec<(String, Vec<Type>)> = Vec::new();
                    for v in &e.variants {
                        let tys: Vec<Type> = v.fields.iter().map(|t| self.resolve_type(t)).collect();
                        if variants.iter().any(|(n, _)| n == &v.name) {
                            self.error("E0201", format!("variant `{}` is defined more than once", v.name), v.span);
                        } else {
                            variants.push((v.name.clone(), tys));
                        }
                    }
                    if !duplicates.contains(&i) {
                        if let Some(info) = self.enums.get_mut(&e.name) {
                            info.variants = variants;
                        }
                    }
                }
                _ => {}
            }
        }

        let mut sigs = Vec::new();
        for item in &program.items {
            let Item::Fn(f) = item else { continue };
            self.tparams = f.type_params.clone();
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
            let names: Vec<String> = f.effects.iter().map(|(e, _)| e.clone()).collect();
            let span = f.effects.first().map_or(f.name_span, |(_, s)| *s);
            let effects = self.check_effect_names(&names, span);
            let sig = FnSig { type_params: f.type_params.clone(), params, ret, effects };
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
        self.tparams.clear();
        sigs
    }

    fn check_fn(&mut self, f: &FnDecl, sig: &FnSig) {
        self.tparams = f.type_params.clone();
        self.scopes = vec![HashMap::new()];
        self.ret = sig.ret.clone();
        self.loop_depth = 0;
        self.closure_base = 0;
        self.infer_effects = None;
        for (p, ty) in f.params.iter().zip(&sig.params) {
            self.scopes[0].insert(p.name.clone(), (ty.clone(), false));
            self.decl_types.push((p.span.start, ty.clone()));
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
                    short(&f.name),
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
        self.tparams.clear();
        self.scopes = vec![HashMap::new()];
        self.ret = Type::Unit;
        self.loop_depth = 0;
        self.closure_base = 0;
        self.infer_effects = None;
        let names: Vec<String> = t.effects.iter().map(|(e, _)| e.clone()).collect();
        self.effects = self.check_effect_names(&names, t.name_span);
        self.block(&t.body, false);
    }

    // ---- scopes ----

    /// Returns (type, mutable, scope index).
    fn lookup(&self, name: &str) -> Option<(Type, bool, usize)> {
        self.scopes
            .iter()
            .enumerate()
            .rev()
            .find_map(|(i, s)| s.get(name).map(|(t, m)| (t.clone(), *m, i)))
    }

    fn declare(&mut self, name: &str, ty: Type, mutable: bool, span: Span) {
        if name == "_" {
            return;
        }
        self.decl_types.push((span.start, ty.clone()));
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
        let name = short(name);
        let mut names: Vec<String> = self.scopes.iter().flat_map(|s| s.keys().cloned()).collect();
        names.extend(self.fns.keys().map(|k| short(k).to_string()));
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
            Stmt::Let { name, name_span, mutable, ty, value, .. } => {
                let declared = match ty {
                    Some(t) => {
                        let annotated = self.resolve_type(t);
                        let value_ty = self.expr_expect(value, Some(&annotated));
                        self.expect(&value_ty, &annotated, value.span);
                        annotated
                    }
                    None => self.expr(value, true),
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
                    match self.lookup(name) {
                        Some((_, false, _)) => self.error_hint(
                            "E0306",
                            format!("cannot assign to `{name}` because it is not mutable"),
                            target.span,
                            format!("declare it with `let mut {name} = ...`"),
                        ),
                        Some((_, true, idx)) if idx < self.closure_base => self.error_hint(
                            "E0313",
                            format!("a closure cannot assign to the captured variable `{name}`"),
                            target.span,
                            "closures capture variables by value; return the new value from the closure instead",
                        ),
                        _ => {}
                    }
                }
                let value_ty = self.expr_expect(value, Some(&target_ty));
                self.expect(&value_ty, &target_ty, value.span);
            }
            Stmt::While { cond, body, .. } => {
                let t = self.expr(cond, true);
                self.expect(&t, &Type::Bool, cond.span);
                self.loop_depth += 1;
                self.block(body, false);
                self.loop_depth -= 1;
            }
            Stmt::For { var, var_span, iter, end, body, .. } => {
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
                            let hint = match self.resolve(&iter_ty) {
                                Type::Map(_, _) => "iterate over `map.keys()` or `map.values()`",
                                _ => "`for` iterates over a List, or over a range written `start..end`",
                            };
                            self.error_hint("E0301", msg, iter.span, hint);
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
            if eff == ANY_EFFECT {
                continue;
            }
            if let Some(used) = &mut self.infer_effects {
                if !used.contains(eff) {
                    used.push(eff.clone());
                }
                continue;
            }
            if !self.effects.contains(eff) {
                let hint = if self.in_contract {
                    "`requires` and `ensures` clauses must be free of side effects".to_string()
                } else {
                    format!("add `uses {eff}` to the signature of the enclosing function or test")
                };
                self.error_hint(
                    "E0401",
                    format!("{callee} uses the `{eff}` effect, which is not declared here"),
                    span,
                    hint,
                );
            }
        }
    }

    /// Checks an expression, using `expected` to infer closure parameter types.
    fn expr_expect(&mut self, e: &Expr, expected: Option<&Type>) -> Type {
        match &e.kind {
            ExprKind::Closure { params, body, .. } => self.closure(params, body, expected),
            _ => self.expr(e, true),
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
            let Some(param) = params.get(i) else {
                self.expr(arg, true);
                continue;
            };
            let t = self.expr_expect(arg, Some(param));
            // Function parameters of built-in methods accept any effects; the
            // effects of the function passed in are charged to the caller.
            if let Type::Fn(eparams, eret, eeffects) = self.resolve(param) {
                if eeffects.iter().any(|e| e == ANY_EFFECT) {
                    match self.resolve(&t) {
                        Type::Fn(aparams, aret, aeffects) => {
                            let ok = self.unify_all(&aparams, &eparams) & self.unify(&aret, &eret);
                            if !ok {
                                let msg = format!(
                                    "type mismatch: expected `{}`, found `{}`",
                                    self.show(param),
                                    self.show(&t)
                                );
                                self.error("E0301", msg, arg.span);
                            }
                            self.require_effects("the function passed here", &aeffects, arg.span);
                        }
                        Type::Error | Type::Never => {}
                        _ => {
                            let msg = format!(
                                "type mismatch: expected `{}`, found `{}`",
                                self.show(param),
                                self.show(&t)
                            );
                            self.error_hint("E0301", msg, arg.span, "pass a closure such as `|x| x + 1`");
                        }
                    }
                    continue;
                }
            }
            self.expect(&t, param, arg.span);
        }
    }

    fn closure(&mut self, params: &[ClosureParam], body: &Expr, expected: Option<&Type>) -> Type {
        let (exp_params, exp_ret) = match expected.map(|t| self.resolve(t)) {
            Some(Type::Fn(ps, r, _)) if ps.len() == params.len() => (Some(ps), Some(*r)),
            _ => (None, None),
        };
        let mut param_tys = Vec::new();
        for (i, p) in params.iter().enumerate() {
            let hinted = exp_params.as_ref().map(|ps| ps[i].clone());
            let ty = match (&p.ty, hinted) {
                (Some(annotation), hinted) => {
                    let ty = self.resolve_type(annotation);
                    if let Some(h) = hinted {
                        self.unify(&ty, &h);
                    }
                    ty
                }
                (None, Some(h)) => h,
                (None, None) => self.fresh(),
            };
            param_tys.push(ty);
        }

        let saved_ret = std::mem::replace(&mut self.ret, Type::Unit);
        self.ret = match exp_ret {
            Some(r) => r,
            None => self.fresh(),
        };
        let saved_infer = self.infer_effects.replace(Vec::new());
        let saved_base = std::mem::replace(&mut self.closure_base, self.scopes.len());
        let saved_loops = std::mem::replace(&mut self.loop_depth, 0);

        self.scopes.push(HashMap::new());
        for (p, ty) in params.iter().zip(&param_tys) {
            self.declare(&p.name, ty.clone(), false, p.span);
        }
        let ret = self.ret.clone();
        let want = self.resolve(&ret) != Type::Unit;
        if want {
            let body_ty = self.expr(body, true);
            if !self.unify(&body_ty, &ret) {
                let msg = format!(
                    "type mismatch: this closure should return `{}`, but its body has type `{}`",
                    self.show(&ret),
                    self.show(&body_ty)
                );
                self.error("E0301", msg, body.span);
            }
        } else {
            self.expr_stmt(body);
        }
        self.scopes.pop();

        let mut effects = std::mem::replace(&mut self.infer_effects, saved_infer).unwrap_or_default();
        effects.sort();
        self.ret = saved_ret;
        self.closure_base = saved_base;
        self.loop_depth = saved_loops;
        Type::Fn(param_tys, Box::new(ret), effects)
    }

    fn unknown_type_error(&mut self, span: Span) {
        self.error_hint(
            "E0311",
            "the type of this value is not known yet",
            span,
            "add a type annotation, e.g. `let x: Int = ...` or a closure parameter `|x: Int|`",
        );
    }

    /// `want` is false when the value of the expression is discarded.
    fn expr(&mut self, e: &Expr, want: bool) -> Type {
        match &e.kind {
            ExprKind::Int(_) => Type::Int,
            ExprKind::Float(_) => Type::Float,
            ExprKind::Bool(_) => Type::Bool,
            ExprKind::Unit => Type::Unit,
            ExprKind::Str(parts) => {
                for part in parts {
                    if let StrPart::Expr(inner) = part {
                        self.expr(inner, true);
                    }
                }
                Type::Str
            }
            ExprKind::Ident(name) => match self.lookup(name) {
                Some((ty, _, _)) => ty,
                None => {
                    self.undefined_variable(name, e.span);
                    Type::Error
                }
            },
            ExprKind::FnRef(name) => match self.fns.get(name).cloned() {
                Some(sig) => {
                    let map = self.instantiate(&sig.type_params);
                    let params = sig.params.iter().map(|p| substitute(p, &map)).collect();
                    Type::Fn(params, Box::new(substitute(&sig.ret, &map)), sig.effects.clone())
                }
                None if BUILTIN_FNS.contains(&name.as_str()) => {
                    self.error_hint(
                        "E0317",
                        format!("the built-in function `{name}` cannot be used as a value"),
                        e.span,
                        format!("wrap it in a closure, e.g. `|x| {name}(x)`"),
                    );
                    Type::Error
                }
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
            ExprKind::MapLit(entries) => {
                let key = self.fresh();
                let value = self.fresh();
                for (k, v) in entries {
                    let kt = self.expr(k, true);
                    self.expect(&kt, &key, k.span);
                    let vt = self.expr(v, true);
                    self.expect(&vt, &value, v.span);
                }
                self.check_map_key(&key, e.span);
                Type::Map(Box::new(key), Box::new(value))
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
                        Type::Var(_) => {
                            self.unknown_type_error(operand.span);
                            Type::Error
                        }
                        _ => {
                            let msg = format!("cannot negate a value of type `{}`", self.show(&t));
                            self.error("E0302", msg, e.span);
                            Type::Error
                        }
                    },
                }
            }
            ExprKind::Binary(op, lhs, rhs) => self.binary(*op, lhs, rhs, e.span),
            ExprKind::Call { name, name_span, local, args } => self.call(name, *name_span, *local, args, e.span),
            ExprKind::MethodCall { recv, name, name_span, args } => {
                let recv_ty = self.expr(recv, true);
                self.method(&recv_ty, name, *name_span, args, e.span)
            }
            ExprKind::Field(base, name, name_span) => {
                let base_ty = self.expr(base, true);
                match self.resolve(&base_ty) {
                    Type::Struct(sname, targs) => {
                        let info = self.structs.get(&sname).cloned().unwrap_or_default();
                        let map: HashMap<String, Type> = info.params.iter().cloned().zip(targs).collect();
                        match info.fields.iter().position(|(n, _)| n == name) {
                            Some(at) => {
                                self.field_uses.push(((e.span.start, e.span.end), at as u32));
                                substitute(&info.fields[at].1, &map)
                            }
                            None => {
                                let names: Vec<String> = info.fields.iter().map(|(n, _)| n.clone()).collect();
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
                        self.unknown_type_error(base.span);
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
                if let Type::Map(key, value) = self.resolve(&base_ty) {
                    self.expect(&index_ty, &key, index.span);
                    return *value;
                }
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
                let Some(info) = self.structs.get(name).cloned() else {
                    let known: Vec<String> = self.structs.keys().map(|k| short(k).to_string()).collect();
                    let mut d = Diagnostic::new("E0202", format!("unknown struct `{name}`"), e.span);
                    if let Some(s) = closest(short(name), known.iter()) {
                        d = d.hint(format!("did you mean `{s}`?"));
                    }
                    self.diags.push(d);
                    for (_, _, value) in fields {
                        self.expr(value, true);
                    }
                    return Type::Error;
                };
                let map = self.instantiate(&info.params);
                let mut given: Vec<&str> = Vec::new();
                for (fname, fspan, value) in fields {
                    if given.contains(&fname.as_str()) {
                        self.expr(value, true);
                        self.error("E0201", format!("field `{fname}` is given more than once"), *fspan);
                        continue;
                    }
                    given.push(fname);
                    match info.fields.iter().find(|(n, _)| n == fname) {
                        Some((_, ty)) => {
                            let expected = substitute(ty, &map);
                            let t = self.expr_expect(value, Some(&expected));
                            self.expect(&t, &expected, value.span);
                        }
                        None => {
                            self.expr(value, true);
                            let names: Vec<String> = info.fields.iter().map(|(n, _)| n.clone()).collect();
                            self.error_hint(
                                "E0304",
                                format!("struct `{name}` has no field `{fname}`"),
                                *fspan,
                                format!("fields of `{name}`: {}", names.join(", ")),
                            );
                        }
                    }
                }
                let missing: Vec<&str> = info
                    .fields
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
                let targs = info.params.iter().map(|p| map[p].clone()).collect();
                Type::Struct(name.clone(), targs)
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
                self.check_exhaustive(arms, &scrut_ty, scrutinee.span);
                if want {
                    result.unwrap_or(Type::Never)
                } else {
                    Type::Unit
                }
            }
            ExprKind::Block(b) => self.block(b, want),
            ExprKind::Closure { params, body, .. } => self.closure(params, body, None),
            ExprKind::Return(value) => {
                let ret = self.ret.clone();
                match value {
                    Some(v) => {
                        let t = self.expr_expect(v, Some(&ret));
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
        self.op_types.push(((span.start, span.end), l.clone()));
        match op {
            BinOp::And | BinOp::Or => {
                self.expect(&l, &Type::Bool, lhs.span);
                self.expect(&r, &Type::Bool, rhs.span);
                Type::Bool
            }
            BinOp::Eq | BinOp::Ne => {
                self.expect(&r, &l, rhs.span);
                if let Type::Fn(..) = self.resolve(&l) {
                    self.error("E0302", "functions cannot be compared", span);
                }
                Type::Bool
            }
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                self.expect(&r, &l, rhs.span);
                match self.resolve(&l) {
                    Type::Int | Type::Float | Type::Str | Type::Error | Type::Never => {}
                    Type::Var(_) => self.unknown_type_error(lhs.span),
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
                // An unknown left operand takes its type from the right one.
                if let (Type::Var(_), Type::Int | Type::Float) = (self.resolve(&l), self.resolve(&r)) {
                    self.unify(&l, &r);
                }
                match self.resolve(&l) {
                    Type::Int | Type::Float => {
                        self.expect(&r, &l, rhs.span);
                        l
                    }
                    Type::Error | Type::Never => Type::Error,
                    Type::Var(_) => {
                        self.unknown_type_error(lhs.span);
                        Type::Error
                    }
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

    fn call(&mut self, name: &str, name_span: Span, local: bool, args: &[Expr], span: Span) -> Type {
        let skip_args = |c: &mut Checker| {
            for arg in args {
                c.expr(arg, true);
            }
        };
        if local {
            let Some((ty, _, _)) = self.lookup(name) else {
                self.undefined_variable(name, name_span);
                skip_args(self);
                return Type::Error;
            };
            return match self.resolve(&ty) {
                Type::Fn(params, ret, effects) => {
                    self.require_effects(&format!("`{name}`"), &effects, span);
                    self.check_args(&format!("`{name}`"), &params, args, span);
                    *ret
                }
                Type::Error | Type::Never => {
                    skip_args(self);
                    Type::Error
                }
                Type::Var(_) => {
                    self.unknown_type_error(name_span);
                    skip_args(self);
                    Type::Error
                }
                _ => {
                    let msg = format!("`{name}` has type `{}` and cannot be called", self.show(&ty));
                    self.error("E0316", msg, name_span);
                    skip_args(self);
                    Type::Error
                }
            };
        }

        let string = || Type::Str;
        let result = |ok: Type| Type::Result(Box::new(ok), Box::new(Type::Str));
        let builtin: Option<(Vec<Type>, Type, &[&str])> = match name {
            "print" => Some((vec![string()], Type::Unit, &["io"])),
            "read_line" => Some((vec![], string(), &["io"])),
            "args" => Some((vec![], Type::List(Box::new(string())), &["io"])),
            "panic" => Some((vec![string()], Type::Never, &[])),
            "read_file" => Some((vec![string()], result(string()), &["fs"])),
            "write_file" => Some((vec![string(), string()], result(Type::Unit), &["fs"])),
            _ => None,
        };
        let (params, ret, effects) = match builtin {
            Some((params, ret, effects)) => (params, ret, effects.iter().map(|s| s.to_string()).collect()),
            None => match self.fns.get(name).cloned() {
                Some(sig) => {
                    let map = self.instantiate(&sig.type_params);
                    let params: Vec<Type> = sig.params.iter().map(|p| substitute(p, &map)).collect();
                    (params, substitute(&sig.ret, &map), sig.effects)
                }
                None => {
                    let mut names: Vec<String> = self.fns.keys().map(|k| short(k).to_string()).collect();
                    names.extend(BUILTIN_FNS.iter().map(|s| s.to_string()));
                    let mut d =
                        Diagnostic::new("E0203", format!("undefined function `{}`", short(name)), name_span);
                    if let Some(s) = closest(short(name), names.iter()) {
                        d = d.hint(format!("did you mean `{s}`?"));
                    }
                    self.diags.push(d);
                    skip_args(self);
                    return Type::Error;
                }
            },
        };
        self.require_effects(&format!("`{}`", short(name)), &effects, span);
        self.check_args(&format!("`{}`", short(name)), &params, args, span);
        ret
    }

    fn method(&mut self, recv_ty: &Type, name: &str, name_span: Span, args: &[Expr], span: Span) -> Type {
        let recv = self.resolve(recv_ty);
        let skip_args = |c: &mut Checker| {
            for arg in args {
                c.expr(arg, true);
            }
        };
        match &recv {
            Type::Error | Type::Never => {
                skip_args(self);
                return Type::Error;
            }
            Type::Var(_) if name != "to_string" => {
                self.unknown_type_error(span);
                skip_args(self);
                return Type::Error;
            }
            _ => {}
        }

        let u = self.fresh();
        let list = |t: &Type| Type::List(Box::new(t.clone()));
        let option = |t: &Type| Type::Option(Box::new(t.clone()));
        let func = |params: Vec<Type>, ret: Type| Type::Fn(params, Box::new(ret), vec![ANY_EFFECT.to_string()]);
        let sig: Option<(Vec<Type>, Type)> = match (&recv, name) {
            (_, "to_string") => Some((vec![], Type::Str)),

            (Type::Str, "len") => Some((vec![], Type::Int)),
            (Type::Str, "is_empty") => Some((vec![], Type::Bool)),
            (Type::Str, "contains" | "starts_with" | "ends_with") => Some((vec![Type::Str], Type::Bool)),
            (Type::Str, "trim" | "to_upper" | "to_lower") => Some((vec![], Type::Str)),
            (Type::Str, "replace") => Some((vec![Type::Str, Type::Str], Type::Str)),
            (Type::Str, "split") => Some((vec![Type::Str], list(&Type::Str))),
            (Type::Str, "chars") => Some((vec![], list(&Type::Str))),
            (Type::Str, "codes") => Some((vec![], list(&Type::Int))),
            (Type::Str, "slice") => Some((vec![Type::Int, Type::Int], Type::Str)),
            (Type::Str, "to_int") => Some((vec![], option(&Type::Int))),
            (Type::Str, "to_float") => Some((vec![], option(&Type::Float))),

            (Type::List(_), "len") => Some((vec![], Type::Int)),
            (Type::List(_), "is_empty") => Some((vec![], Type::Bool)),
            (Type::List(t), "push") => Some((vec![(**t).clone()], list(t))),
            (Type::List(t), "get") => Some((vec![Type::Int], option(t))),
            (Type::List(t), "contains") => Some((vec![(**t).clone()], Type::Bool)),
            (Type::List(t), "first" | "last") => Some((vec![], option(t))),
            (Type::List(t), "concat") => Some((vec![list(t)], list(t))),
            (Type::List(t), "reverse") => Some((vec![], list(t))),
            (Type::List(t), "slice") => Some((vec![Type::Int, Type::Int], list(t))),
            (Type::List(t), "sort") => {
                match self.resolve(t) {
                    Type::Int | Type::Float | Type::Str | Type::Error | Type::Var(_) => {}
                    _ => {
                        let msg = format!("`sort` cannot order values of type `{}`", self.show(t));
                        self.error_hint("E0302", msg, name_span, "sort works on lists of Int, Float or String");
                    }
                }
                Some((vec![], list(t)))
            }
            (Type::List(t), "join") => {
                if !self.unify(t, &Type::Str) {
                    let msg = format!("`join` needs a List<String>, found `{}`", self.show(&recv));
                    self.error_hint("E0302", msg, name_span, "convert the items first: `.map(|x| x.to_string())`");
                }
                Some((vec![Type::Str], Type::Str))
            }
            (Type::List(t), "map") => Some((vec![func(vec![(**t).clone()], u.clone())], list(&u))),
            (Type::List(t), "filter") => Some((vec![func(vec![(**t).clone()], Type::Bool)], list(t))),
            (Type::List(t), "any" | "all") => Some((vec![func(vec![(**t).clone()], Type::Bool)], Type::Bool)),
            (Type::List(t), "fold") => {
                Some((vec![u.clone(), func(vec![u.clone(), (**t).clone()], u.clone())], u.clone()))
            }

            (Type::Map(_, _), "len") => Some((vec![], Type::Int)),
            (Type::Map(_, _), "is_empty") => Some((vec![], Type::Bool)),
            (Type::Map(k, v), "get") => Some((vec![(**k).clone()], option(v))),
            (Type::Map(k, _), "has") => Some((vec![(**k).clone()], Type::Bool)),
            (Type::Map(k, v), "set") => Some((vec![(**k).clone(), (**v).clone()], recv.clone())),
            (Type::Map(k, _), "remove") => Some((vec![(**k).clone()], recv.clone())),
            (Type::Map(k, _), "keys") => Some((vec![], list(k))),
            (Type::Map(_, v), "values") => Some((vec![], list(v))),

            (Type::Int, "to_float") => Some((vec![], Type::Float)),
            (Type::Int, "abs") => Some((vec![], Type::Int)),
            (Type::Int, "min" | "max") => Some((vec![Type::Int], Type::Int)),
            (Type::Int, "bit_and" | "bit_or" | "bit_xor" | "shl" | "shr") => Some((vec![Type::Int], Type::Int)),
            (Type::Int, "to_char") => Some((vec![], Type::Str)),
            (Type::Float, "to_int") => Some((vec![], Type::Int)),
            (Type::Float, "abs" | "sqrt" | "floor" | "ceil" | "round") => Some((vec![], Type::Float)),
            (Type::Float, "min" | "max") => Some((vec![Type::Float], Type::Float)),

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
                    Type::Str => {
                        "len, is_empty, contains, starts_with, ends_with, trim, to_upper, to_lower, replace, split, chars, codes, slice, to_int, to_float, to_string"
                    }
                    Type::List(_) => {
                        "len, is_empty, push, get, contains, first, last, concat, reverse, slice, sort, join, map, filter, fold, any, all, to_string"
                    }
                    Type::Map(_, _) => "len, is_empty, get, has, set, remove, keys, values, to_string",
                    Type::Int => "to_float, abs, min, max, bit_and, bit_or, bit_xor, shl, shr, to_char, to_string",
                    Type::Float => "to_int, abs, sqrt, floor, ceil, round, min, max, to_string",
                    Type::Option(_) => "unwrap_or, is_some, is_none, to_string",
                    Type::Result(_, _) => "unwrap_or, is_ok, is_err, to_string",
                    _ => "to_string",
                };
                let msg = format!("type `{}` has no method `{name}`", self.show(&recv));
                self.error_hint("E0304", msg, name_span, format!("available methods: {available}"));
                skip_args(self);
                Type::Error
            }
        }
    }

    /// Looks up a user enum variant. Returns the enum type and the field types.
    fn variant(&mut self, enum_name: &str, variant: &str, span: Span) -> Option<(Type, Vec<Type>)> {
        let Some(info) = self.enums.get(enum_name).cloned() else {
            let known: Vec<String> = self.enums.keys().map(|k| short(k).to_string()).collect();
            let mut d = Diagnostic::new("E0202", format!("unknown enum `{enum_name}`"), span);
            if let Some(s) = closest(short(enum_name), known.iter()) {
                d = d.hint(format!("did you mean `{s}`?"));
            }
            self.diags.push(d);
            return None;
        };
        let map = self.instantiate(&info.params);
        let targs: Vec<Type> = info.params.iter().map(|p| map[p].clone()).collect();
        let ty = Type::Enum(enum_name.to_string(), targs);
        match info.variants.iter().find(|(n, _)| n == variant) {
            Some((_, fields)) => Some((ty, fields.iter().map(|f| substitute(f, &map)).collect())),
            None => {
                let names: Vec<String> = info.variants.iter().map(|(n, _)| n.clone()).collect();
                self.error_hint(
                    "E0304",
                    format!("enum `{enum_name}` has no variant `{variant}`"),
                    span,
                    format!("variants of `{enum_name}`: {}", names.join(", ")),
                );
                None
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
            _ => match self.variant(enum_name, variant, span) {
                Some((ty, fields)) => {
                    self.check_args(&format!("`{}.{variant}`", short(enum_name)), &fields, args, span);
                    ty
                }
                None => {
                    for arg in args {
                        self.expr(arg, true);
                    }
                    Type::Error
                }
            },
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
                    _ => match self.variant(enum_name, variant, pat.span) {
                        Some(found) => found,
                        None => return,
                    },
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

    // ---- exhaustiveness (pattern matrix algorithm) ----

    /// The complete set of constructors of a type, or None when it is open-ended.
    fn ctors_of(&self, ty: &Type) -> Option<Vec<Ctor>> {
        let ctor = |key: &str, display: String, fields: Vec<Type>| Ctor { key: key.to_string(), display, fields };
        match self.resolve(ty) {
            Type::Bool => Some(vec![ctor("true", "true".into(), vec![]), ctor("false", "false".into(), vec![])]),
            Type::Option(t) => Some(vec![ctor("Some", "Some".into(), vec![*t]), ctor("None", "None".into(), vec![])]),
            Type::Result(t, e) => {
                Some(vec![ctor("Ok", "Ok".into(), vec![*t]), ctor("Err", "Err".into(), vec![*e])])
            }
            Type::Enum(name, targs) => {
                let info = self.enums.get(&name)?;
                let map: HashMap<String, Type> = info.params.iter().cloned().zip(targs).collect();
                Some(
                    info.variants
                        .iter()
                        .map(|(v, fields)| {
                            let fields = fields.iter().map(|f| substitute(f, &map)).collect();
                            ctor(v, format!("{}.{v}", short(&name)), fields)
                        })
                        .collect(),
                )
            }
            _ => None,
        }
    }

    fn simplify(pat: &Pattern) -> SPat {
        match &pat.kind {
            PatKind::Wildcard | PatKind::Bind(_) => SPat::Wild,
            PatKind::Int(n) => SPat::Con(format!("#{n}"), vec![]),
            PatKind::Str(s) => SPat::Con(format!("\"{s}"), vec![]),
            PatKind::Bool(b) => SPat::Con(b.to_string(), vec![]),
            PatKind::Ctor { variant, args, .. } => {
                SPat::Con(variant.clone(), args.iter().map(Checker::simplify).collect())
            }
        }
    }

    /// Rows that can match a value built with constructor `key`.
    fn specialize(rows: &[Vec<SPat>], key: &str, arity: usize) -> Vec<Vec<SPat>> {
        let mut out = Vec::new();
        for row in rows {
            let mut new_row = match &row[0] {
                SPat::Wild => vec![SPat::Wild; arity],
                SPat::Con(k, args) if k == key => {
                    let mut args = args.clone();
                    args.resize(arity, SPat::Wild);
                    args
                }
                SPat::Con(..) => continue,
            };
            new_row.extend_from_slice(&row[1..]);
            out.push(new_row);
        }
        out
    }

    fn default_rows(rows: &[Vec<SPat>]) -> Vec<Vec<SPat>> {
        rows.iter().filter(|r| matches!(r[0], SPat::Wild)).map(|r| r[1..].to_vec()).collect()
    }

    fn heads(rows: &[Vec<SPat>]) -> HashSet<&str> {
        rows.iter()
            .filter_map(|r| match &r[0] {
                SPat::Con(k, _) => Some(k.as_str()),
                SPat::Wild => None,
            })
            .collect()
    }

    /// Whether `q` matches some value that none of `rows` matches.
    fn useful(&self, rows: &[Vec<SPat>], q: &[SPat], tys: &[Type]) -> bool {
        if q.is_empty() || tys.is_empty() {
            return rows.is_empty();
        }
        let sig = self.ctors_of(&tys[0]);
        match &q[0] {
            SPat::Con(key, args) => {
                let fields = sig
                    .and_then(|s| s.into_iter().find(|c| &c.key == key))
                    .map(|c| c.fields)
                    .unwrap_or_default();
                if fields.len() != args.len() {
                    return true;
                }
                let rows = Checker::specialize(rows, key, args.len());
                let q: Vec<SPat> = args.iter().chain(&q[1..]).cloned().collect();
                let tys: Vec<Type> = fields.iter().chain(&tys[1..]).cloned().collect();
                self.useful(&rows, &q, &tys)
            }
            SPat::Wild => {
                let heads = Checker::heads(rows);
                match sig {
                    Some(sig) if sig.iter().all(|c| heads.contains(c.key.as_str())) => sig.iter().any(|c| {
                        let rows = Checker::specialize(rows, &c.key, c.fields.len());
                        let q: Vec<SPat> =
                            std::iter::repeat(SPat::Wild).take(c.fields.len()).chain(q[1..].iter().cloned()).collect();
                        let tys: Vec<Type> = c.fields.iter().chain(&tys[1..]).cloned().collect();
                        self.useful(&rows, &q, &tys)
                    }),
                    _ => self.useful(&Checker::default_rows(rows), &q[1..], &tys[1..]),
                }
            }
        }
    }

    /// An example of values (one per column) that no row matches, if any.
    fn witness(&self, rows: &[Vec<SPat>], tys: &[Type]) -> Option<Vec<String>> {
        if tys.is_empty() {
            return if rows.is_empty() { Some(Vec::new()) } else { None };
        }
        let sig = self.ctors_of(&tys[0]);
        let heads = Checker::heads(rows);
        if let Some(sig) = &sig {
            if sig.iter().all(|c| heads.contains(c.key.as_str())) {
                for c in sig {
                    let rows = Checker::specialize(rows, &c.key, c.fields.len());
                    let tys: Vec<Type> = c.fields.iter().chain(&tys[1..]).cloned().collect();
                    if let Some(w) = self.witness(&rows, &tys) {
                        let (args, rest) = w.split_at(c.fields.len());
                        let head = if args.is_empty() {
                            c.display.clone()
                        } else {
                            format!("{}({})", c.display, args.join(", "))
                        };
                        return Some(std::iter::once(head).chain(rest.iter().cloned()).collect());
                    }
                }
                return None;
            }
        }
        let rest = self.witness(&Checker::default_rows(rows), &tys[1..])?;
        let head = match sig.and_then(|s| s.into_iter().find(|c| !heads.contains(c.key.as_str()))) {
            Some(c) if c.fields.is_empty() => c.display,
            Some(c) => format!("{}({})", c.display, vec!["_"; c.fields.len()].join(", ")),
            None => "_".to_string(),
        };
        Some(std::iter::once(head).chain(rest).collect())
    }

    fn check_exhaustive(&mut self, arms: &[Arm], scrut_ty: &Type, span: Span) {
        let ty = self.apply(scrut_ty);
        if matches!(ty, Type::Error | Type::Never) {
            return;
        }
        let tys = [ty];
        let mut rows: Vec<Vec<SPat>> = Vec::new();
        for arm in arms {
            let row = vec![Checker::simplify(&arm.pat)];
            if !self.useful(&rows, &row, &tys) {
                self.error_hint(
                    "E0314",
                    "unreachable match arm: earlier arms already cover every value it matches",
                    arm.pat.span,
                    "remove this arm, or move it above the arm that covers it",
                );
            }
            rows.push(row);
        }
        if let Some(w) = self.witness(&rows, &tys) {
            let missing = w.first().cloned().unwrap_or_else(|| "_".to_string());
            self.error_hint(
                "E0308",
                format!("match is not exhaustive: `{missing}` is not covered"),
                span,
                format!("add an arm for `{missing}`, or a catch-all arm `_ => ...`"),
            );
        }
    }
}
