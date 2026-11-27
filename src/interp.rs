//! Runs programs that have passed the type checker.
//!
//! `bytecode` compiles every function to a flat list of ops that work on a
//! value stack; local variables live in numbered slots at the bottom of each
//! call's part of that stack. On x86-64 the ops are then translated to machine
//! code by `jit`, which calls back into `exec` here for everything it does not
//! generate itself. Elsewhere, and when `Interp::jit` is off, `run` interprets
//! the ops directly.

use crate::ast::*;
use crate::bytecode::{self, Func, Globals, InPlace, Op, PathElem, NONE};
use crate::diag::{Diagnostic, Span};
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::rc::Rc;

/// Maximum depth of nested function calls.
pub const MAX_DEPTH: usize = 10_000;

/// A runtime value. The layout is fixed because generated machine code reads
/// and writes values directly: the discriminant is the first byte, an Int's
/// number is at offset 8 and a Bool's flag at offset 1. Variants below
/// `Str` own no heap memory, so that code may copy and overwrite them freely.
#[derive(Clone)]
#[repr(u8)]
pub enum Value {
    Int(i64) = 0,
    Float(f64) = 1,
    Bool(bool) = 2,
    Unit = 3,
    Str(Rc<str>) = 4,
    List(Rc<Vec<Value>>) = 5,
    Map(Rc<BTreeMap<Key, Value>>) = 6,
    Struct(Rc<StructVal>) = 7,
    Enum(Rc<EnumVal>) = 8,
    Fn(Rc<Func>) = 9,
    Closure(Rc<ClosureVal>) = 10,
    /// A list holding nothing but Ints, stored as plain numbers: a third of
    /// the memory of `List`. It is the same list as far as programs can tell;
    /// it turns into a `List` when something else is put into it.
    Ints(Rc<Vec<i64>>) = 11,
}

/// The items of a packed list as ordinary values.
fn unpack(ints: &[i64]) -> Vec<Value> {
    ints.iter().map(|n| Value::Int(*n)).collect()
}

/// `v`, with a packed list turned into an ordinary one.
fn unpacked(v: Value) -> Value {
    match v {
        Value::Ints(ints) => Value::List(Rc::new(unpack(&ints))),
        other => other,
    }
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
    pub name: Rc<str>,
    pub fields: Vec<(Rc<str>, Value)>,
}

#[derive(Clone, PartialEq)]
pub struct EnumVal {
    pub enum_name: Rc<str>,
    pub variant: Rc<str>,
    pub args: Vec<Value>,
}

/// `#[repr(C)]` because generated code reads both fields.
#[repr(C)]
pub struct ClosureVal {
    func: Rc<Func>,
    /// Copies of the captured variables, in the order the function expects them.
    captured: Vec<Value>,
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
            (Value::Ints(a), Value::Ints(b)) => a == b,
            (Value::Ints(a), Value::List(b)) | (Value::List(b), Value::Ints(a)) => {
                a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| matches!(y, Value::Int(n) if n == x))
            }
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

/// Names of the built-in enums, shared so that building an Option or a Result
/// does not allocate them again.
struct Names {
    option: Rc<str>,
    some: Rc<str>,
    result: Rc<str>,
    ok: Rc<str>,
    err: Rc<str>,
    none: Value,
}

thread_local! {
    static NAMES: Names = Names {
        option: Rc::from("Option"),
        some: Rc::from("Some"),
        result: Rc::from("Result"),
        ok: Rc::from("Ok"),
        err: Rc::from("Err"),
        none: Value::variant(Rc::from("Option"), Rc::from("None"), Vec::new()),
    };
}

impl Value {
    fn str(s: impl AsRef<str>) -> Value {
        Value::Str(Rc::from(s.as_ref()))
    }

    fn list(items: Vec<Value>) -> Value {
        // A list of nothing but Ints is stored as plain numbers.
        if !items.is_empty() && items.iter().all(|v| matches!(v, Value::Int(_))) {
            let ints = items.iter().map(|v| if let Value::Int(n) = v { *n } else { 0 }).collect();
            return Value::Ints(Rc::new(ints));
        }
        Value::List(Rc::new(items))
    }

    pub fn variant(enum_name: Rc<str>, variant: Rc<str>, args: Vec<Value>) -> Value {
        Value::Enum(Rc::new(EnumVal { enum_name, variant, args }))
    }

    fn some(v: Value) -> Value {
        NAMES.with(|n| Value::variant(n.option.clone(), n.some.clone(), vec![v]))
    }

    fn none() -> Value {
        NAMES.with(|n| n.none.clone())
    }

    fn ok(v: Value) -> Value {
        NAMES.with(|n| Value::variant(n.result.clone(), n.ok.clone(), vec![v]))
    }

    fn err(v: Value) -> Value {
        NAMES.with(|n| Value::variant(n.result.clone(), n.err.clone(), vec![v]))
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
            Value::Ints(items) => {
                format!("[{}]", items.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(", "))
            }
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
                let mut out = match &*e.enum_name {
                    "Option" | "Result" => e.variant.to_string(),
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

/// A runtime error. Boxed so that the common, successful path stays small.
type Fault = Box<Diagnostic>;
type R<T> = Result<T, Fault>;

fn panic(code: &'static str, msg: impl Into<String>, span: Span) -> Fault {
    Box::new(Diagnostic::new(code, msg, span))
}

fn panic_hint(code: &'static str, msg: impl Into<String>, span: Span, hint: &str) -> Fault {
    Box::new(Diagnostic::new(code, msg, span).hint(hint))
}

#[cold]
fn internal(span: Span) -> Fault {
    panic("E0599", "internal error: the interpreter reached a state the type checker should prevent", span)
}

fn out_of_bounds(index: i64, len: usize, span: Span) -> Fault {
    panic_hint(
        "E0505",
        format!("index {index} is out of bounds for a list of length {len}"),
        span,
        "use `.get(i)` to receive an Option instead of failing",
    )
}

fn missing_key(key: &Key, span: Span) -> Fault {
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

fn ordering_matches(op: BinOp, o: Ordering) -> bool {
    match op {
        BinOp::Lt => o == Ordering::Less,
        BinOp::Le => o != Ordering::Greater,
        BinOp::Gt => o == Ordering::Greater,
        BinOp::Ge => o != Ordering::Less,
        BinOp::Eq => o == Ordering::Equal,
        BinOp::Ne => o != Ordering::Equal,
        _ => false,
    }
}

fn compare(op: BinOp, l: &Value, r: &Value) -> bool {
    match op {
        BinOp::Eq => l == r,
        BinOp::Ne => l != r,
        _ => order(l, r).is_some_and(|o| ordering_matches(op, o)),
    }
}

/// The overflow-checked operations that cannot fail in any other way.
/// Returns None for the rest, and on overflow, so the caller can take the slow path.
#[inline(always)]
fn int_fast(op: BinOp, a: i64, b: i64) -> Option<i64> {
    match op {
        BinOp::Add => a.checked_add(b),
        BinOp::Sub => a.checked_sub(b),
        BinOp::Mul => a.checked_mul(b),
        _ => None,
    }
}

#[inline(never)]
fn int_arith(op: BinOp, a: i64, b: i64, span: Span) -> R<Value> {
    let result = match op {
        BinOp::Add => a.checked_add(b),
        BinOp::Sub => a.checked_sub(b),
        BinOp::Mul => a.checked_mul(b),
        BinOp::Div | BinOp::Rem if b == 0 => {
            return Err(panic_hint(
                "E0504",
                "division by zero",
                span,
                "check the divisor first, or add a `requires` clause",
            ));
        }
        BinOp::Div => a.checked_div(b),
        BinOp::Rem => a.checked_rem(b),
        _ => return Err(internal(span)),
    };
    match result {
        Some(v) => Ok(Value::Int(v)),
        None => Err(panic("E0503", format!("integer overflow in `{a} {} {b}`", op.symbol()), span)),
    }
}

#[inline(never)]
fn arith(op: BinOp, l: &Value, r: &Value, span: Span) -> R<Value> {
    match (l, r) {
        (Value::Int(a), Value::Int(b)) => int_arith(op, *a, *b, span),
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

fn field_mut<'v>(value: &'v mut Value, name: &str, at: u32, span: Span) -> R<&'v mut Value> {
    // The usual case: the type checker recorded where the field is.
    if at != NONE {
        return match value {
            Value::Struct(s) => {
                Rc::make_mut(s).fields.get_mut(at as usize).map(|(_, v)| v).ok_or_else(|| internal(span))
            }
            _ => Err(internal(span)),
        };
    }
    match value {
        Value::Struct(s) => Rc::make_mut(s)
            .fields
            .iter_mut()
            .find(|(n, _)| **n == *name)
            .map(|(_, v)| v)
            .ok_or_else(|| internal(span)),
        _ => Err(internal(span)),
    }
}

/// Mutable access to `value[index]` for an existing element.
fn element_mut(value: &mut Value, index: Value, span: Span) -> R<&mut Value> {
    // A packed list has no `Value` to point at.
    if let Value::Ints(ints) = value {
        let items = unpack(ints);
        *value = Value::List(Rc::new(items));
    }
    match (value, index) {
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

/// `value[index] = v`. Assigning to a map entry may insert a new key.
fn store_index(value: &mut Value, index: Value, v: Value, span: Span) -> R<()> {
    if let Value::Map(entries) = value {
        let key = to_key(&index, span)?;
        Rc::make_mut(entries).insert(key, v);
        return Ok(());
    }
    if let (Value::Ints(ints), Value::Int(i), Value::Int(n)) = (&mut *value, &index, &v) {
        let len = ints.len();
        if *i < 0 || *i as usize >= len {
            return Err(out_of_bounds(*i, len, span));
        }
        Rc::make_mut(ints)[*i as usize] = *n;
        return Ok(());
    }
    *element_mut(value, index, span)? = v;
    Ok(())
}

/// Follows a path of struct fields down from a variable.
fn fields_mut<'v>(root: &'v mut Value, path: &[PathElem], span: Span) -> R<&'v mut Value> {
    let mut cur = root;
    for elem in path {
        cur = match elem {
            PathElem::Field(name, span, at) => field_mut(cur, name, *at, *span)?,
            PathElem::Index(_) => return Err(internal(span)),
        };
    }
    Ok(cur)
}

/// The value stack: one fixed block of memory, so that generated code can keep
/// addresses into it. `#[repr(C)]` because that code reads and writes `top`.
#[repr(C)]
pub struct Stack {
    base: *mut Value,
    /// Just past the last value.
    top: *mut Value,
    end: *mut Value,
    /// The interpreter that owns this stack, for helpers called by generated code.
    #[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
    owner: *mut u8,
    /// The calls in progress: a second fixed block, so that generated code
    /// can start and finish a call without leaving machine code.
    frames_base: *mut Frame,
    /// Just past the innermost call.
    frames_top: *mut Frame,
    /// Calls may not nest beyond this point.
    frames_limit: *mut Frame,
}

impl Stack {
    const CAPACITY: usize = 1 << 22;
    /// Room for the deepest allowed recursion plus the frame of a test body.
    const FRAME_CAPACITY: usize = MAX_DEPTH + 8;

    fn new() -> Stack {
        let mut block: Vec<Value> = Vec::with_capacity(Self::CAPACITY);
        let base = block.as_mut_ptr();
        std::mem::forget(block);
        // Safety: `base + CAPACITY` is the end of the allocation.
        let end = unsafe { base.add(Self::CAPACITY) };
        let mut frames: Vec<Frame> = Vec::with_capacity(Self::FRAME_CAPACITY);
        let frames_base = frames.as_mut_ptr();
        std::mem::forget(frames);
        Stack {
            base,
            top: base,
            end,
            owner: std::ptr::null_mut(),
            frames_base,
            frames_top: frames_base,
            frames_limit: frames_base,
        }
    }

    fn len(&self) -> usize {
        // Safety: both point into the same allocation, and `top >= base`.
        unsafe { self.top.offset_from(self.base) as usize }
    }

    /// Number of values that can still be pushed.
    fn remaining(&self) -> usize {
        // Safety: as in `len`.
        unsafe { self.end.offset_from(self.top) as usize }
    }

    #[inline]
    fn push(&mut self, v: Value) {
        if self.top == self.end {
            stack_full();
        }
        // Safety: `top` is inside the allocation and holds no live value.
        unsafe {
            self.top.write(v);
            self.top = self.top.add(1);
        }
    }

    #[inline]
    fn pop(&mut self) -> Option<Value> {
        if self.top == self.base {
            return None;
        }
        // Safety: the cell below `top` holds a live value, which is moved out.
        unsafe {
            self.top = self.top.sub(1);
            Some(self.top.read())
        }
    }

    fn truncate(&mut self, len: usize) {
        while self.len() > len {
            self.pop();
        }
    }

    fn clear(&mut self) {
        self.truncate(0);
    }

    /// Removes and returns everything from index `at` up.
    fn split_off(&mut self, at: usize) -> Vec<Value> {
        let count = self.len() - at;
        let mut out = Vec::with_capacity(count);
        // Safety: the top `count` values are live; they are moved into `out`
        // and `top` is lowered so the stack no longer owns them.
        unsafe {
            let from = self.base.add(at);
            std::ptr::copy_nonoverlapping(from, out.as_mut_ptr(), count);
            out.set_len(count);
            self.top = from;
        }
        out
    }

    fn extend(&mut self, items: impl IntoIterator<Item = Value>) {
        for v in items {
            self.push(v);
        }
    }

    fn extend_from_slice(&mut self, items: &[Value]) {
        for v in items {
            self.push(v.clone());
        }
    }
}

impl std::ops::Deref for Stack {
    type Target = [Value];

    fn deref(&self) -> &[Value] {
        // Safety: the cells from `base` up to `top` hold live values.
        unsafe { std::slice::from_raw_parts(self.base, self.len()) }
    }
}

impl std::ops::DerefMut for Stack {
    fn deref_mut(&mut self) -> &mut [Value] {
        // Safety: as in `deref`.
        unsafe { std::slice::from_raw_parts_mut(self.base, self.len()) }
    }
}

impl Drop for Stack {
    fn drop(&mut self) {
        self.clear();
        // Safety: both blocks came from Vecs with these capacities. The value
        // block is now empty, and frames own nothing.
        unsafe {
            drop(Vec::from_raw_parts(self.base, 0, Self::CAPACITY));
            drop(Vec::from_raw_parts(self.frames_base, 0, Self::FRAME_CAPACITY));
        }
    }
}

#[cold]
fn stack_full() -> ! {
    eprintln!("fatal: the value stack is full");
    std::process::abort();
}

/// What the machine does after an op.
enum Flow {
    Next,
    Jump(usize),
    Return(Value),
}

/// One call in progress. `#[repr(C)]` because generated code writes these.
#[repr(C)]
#[derive(Clone, Copy)]
struct Frame {
    /// The function being run. `Globals::funcs` keeps every function alive
    /// for as long as the interpreter exists; `start` does so for a test body.
    func: *const Func,
    /// Where to continue in `func` once the call it made returns.
    pc: usize,
    /// Address of the function's first local slot.
    base: *mut Value,
    /// Where the function was called from, for contract violations.
    span_start: usize,
    span_end: usize,
}

impl Frame {
    fn func<'f>(&self) -> &'f Func {
        // Safety: see the field.
        unsafe { &*self.func }
    }

    fn call_span(&self) -> Span {
        Span::new(self.span_start, self.span_end)
    }

    /// Stack index of the function's first local slot.
    fn base_index(&self, stack: &Stack) -> usize {
        // Safety: `base` points into the value stack.
        unsafe { self.base.offset_from(stack.base) as usize }
    }
}

pub struct Interp<'a> {
    src: &'a str,
    g: Globals,
    main: Option<(u32, Span)>,
    stack: Stack,
    /// The error that made generated code stop.
    #[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
    fault: Option<Fault>,
    #[cfg(target_arch = "x86_64")]
    code: crate::jit::CodeArena,
    /// Result caches of memoized functions; generated code holds their addresses.
    #[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
    memo_tables: Vec<Box<[u64]>>,
    /// Run generated machine code instead of interpreting the bytecode.
    /// Only available on x86-64; set `YUI_JIT=0` to turn it off.
    pub jit: bool,

    /// Command line arguments returned by `args()`.
    pub args: Vec<String>,
    /// When set, `print` appends here instead of writing to stdout.
    pub captured: Option<String>,
}

impl<'a> Interp<'a> {
    /// `src` is the text of the source map the program was parsed from.
    pub fn new(program: &Program, src: &'a str) -> Interp<'a> {
        let mut g = Globals { types: program.types.clone(), ..Globals::default() };
        let mut decls = Vec::new();
        let mut main = None;
        for item in &program.items {
            match item {
                Item::Fn(f) => {
                    if !g.fn_index.contains_key(&f.name) {
                        let index = g.funcs.len() as u32;
                        g.fn_index.insert(f.name.clone(), index);
                        g.funcs.push(Rc::new(Func::default()));
                        if f.name == "main" {
                            main = Some((index, f.name_span));
                        }
                        decls.push((index, f));
                    }
                }
                Item::Struct(s) => {
                    g.struct_fields.insert(s.name.clone(), s.fields.iter().map(|f| f.name.clone()).collect());
                }
                _ => {}
            }
        }
        for (index, f) in decls {
            let func = bytecode::compile_fn(&mut g, f);
            g.funcs[index as usize] = Rc::new(func);
        }
        let mut interp = Interp {
            src,
            g,
            main,
            stack: Stack::new(),
            memo_tables: Vec::new(),
            fault: None,
            #[cfg(target_arch = "x86_64")]
            code: Default::default(),
            jit: false,
            args: Vec::new(),
            captured: None,
        };
        interp.jit = std::env::var_os("YUI_JIT").is_none_or(|v| v != "0") && interp.native_available();
        interp
    }

    pub fn run_main(&mut self) -> Result<(), Diagnostic> {
        let Some((index, span)) = self.main else {
            return Err(Diagnostic::new("E0207", "no `main` function found", Span::new(0, 0))
                .hint("add `fn main() uses io { ... }`"));
        };
        let func = self.g.funcs[index as usize].clone();
        self.start(&func, 0, span)
    }

    pub fn run_test(&mut self, test: &TestDecl) -> Result<(), Diagnostic> {
        let func = Rc::new(bytecode::compile_test(&mut self.g, test));
        self.start(&func, 1, test.name_span)
    }

    /// Runs `func` from an empty stack. `free_frames` is the number of frames
    /// that do not count as a call: 1 for a test body.
    fn start(&mut self, func: &Rc<Func>, free_frames: usize, span: Span) -> Result<(), Diagnostic> {
        self.stack.clear();
        self.stack.frames_top = self.stack.frames_base;
        // Safety: `FRAME_CAPACITY` leaves room for `MAX_DEPTH` plus the free frames.
        self.stack.frames_limit = unsafe { self.stack.frames_base.add(MAX_DEPTH + free_frames) };
        let result = self.push_frame(Rc::as_ptr(func), 0, &[], span).and_then(|_| self.enter());
        self.stack.clear();
        self.stack.frames_top = self.stack.frames_base;
        result.map(|_| ()).map_err(|d| *d)
    }

    /// Number of calls in progress.
    fn depth(&self) -> usize {
        // Safety: both point into the frame block, and `frames_top >= frames_base`.
        unsafe { self.stack.frames_top.offset_from(self.stack.frames_base) as usize }
    }

    /// The innermost call.
    #[inline]
    fn frame(&self) -> Frame {
        debug_assert!(self.depth() > 0);
        // Safety: a call is in progress whenever code runs.
        unsafe { *self.stack.frames_top.sub(1) }
    }

    fn set_pc(&mut self, pc: usize) {
        // Safety: as in `frame`.
        unsafe { (*self.stack.frames_top.sub(1)).pc = pc };
    }

    fn text(&self, span: Span) -> &'a str {
        self.src.get(span.start..span.end).unwrap_or("")
    }

    // ---- calls ----

    /// Starts a call whose `argc` arguments are on top of the stack.
    #[inline]
    fn push_frame(&mut self, callee: *const Func, argc: usize, captured: &[Value], span: Span) -> R<()> {
        // Safety: callers pass a function that outlives the call (see `Frame::func`).
        let func = unsafe { &*callee };
        if self.stack.frames_top >= self.stack.frames_limit {
            return Err(panic_hint(
                "E0506",
                format!("stack overflow: more than {MAX_DEPTH} nested calls"),
                span,
                "check that the recursion has a base case",
            ));
        }
        // Generated code pushes temporaries without checking; a function never
        // needs more of them than it has ops.
        if self.stack.remaining() < func.nlocals + func.code.len() + 64 {
            return Err(panic_hint(
                "E0506",
                "stack overflow: the value stack is full",
                span,
                "check that the recursion has a base case",
            ));
        }
        let base = self.stack.len() - argc;
        if !captured.is_empty() {
            self.stack.extend_from_slice(captured);
        }
        for _ in self.stack.len()..base + func.nlocals {
            self.stack.push(Value::Unit);
        }
        // Safety: `frames_top < frames_limit`, which is inside the frame block,
        // and `base` is at most the stack length.
        unsafe {
            let frame = Frame {
                func: callee,
                pc: 0,
                base: self.stack.base.add(base),
                span_start: span.start,
                span_end: span.end,
            };
            self.stack.frames_top.write(frame);
            self.stack.frames_top = self.stack.frames_top.add(1);
        }
        Ok(())
    }

    /// Starts a call to a function value (a named function or a closure).
    fn push_value_frame(&mut self, f: &Value, argc: usize, span: Span) -> R<()> {
        match f {
            Value::Fn(func) => self.push_frame(Rc::as_ptr(func), argc, &[], span),
            Value::Closure(c) => self.push_frame(Rc::as_ptr(&c.func), argc, &c.captured, span),
            _ => Err(internal(span)),
        }
    }

    /// Calls a function value from native code and waits for its result.
    /// The arguments are passed as an array so that no call allocates.
    fn call_value<const N: usize>(&mut self, f: &Value, args: [Value; N], span: Span) -> R<Value> {
        let depth = self.depth();
        let argc = N;
        self.stack.extend(args);
        self.push_value_frame(f, argc, span)?;
        if self.jit {
            self.enter_native()
        } else {
            self.run(depth)
        }
    }

    /// Runs the frame that was just pushed and returns its result.
    fn enter(&mut self) -> R<Value> {
        if self.jit {
            self.enter_native()
        } else {
            self.run(self.depth() - 1)
        }
    }

    /// Ends the current call, leaving `value` on the stack as its result.
    fn finish_call(&mut self, value: Value) {
        let frame = self.frame();
        // Safety: a call is in progress, so there is a frame to remove.
        self.stack.frames_top = unsafe { self.stack.frames_top.sub(1) };
        let value = if frame.func().unit_ret { Value::Unit } else { value };
        self.stack.truncate(frame.base_index(&self.stack));
        self.stack.push(value);
    }

    #[inline]
    fn pop(&mut self) -> Value {
        self.stack.pop().expect("value stack is empty")
    }

    // ---- the machine ----

    /// Interprets the top frame until the call stack is back down to `stop`
    /// frames, and returns the value that last call returned.
    fn run(&mut self, stop: usize) -> R<Value> {
        let (mut func, mut pc, mut base) = {
            let frame = self.frame();
            (frame.func(), frame.pc, frame.base_index(&self.stack))
        };
        // Continue in whichever frame is now on top.
        macro_rules! resume {
            () => {{
                let frame = self.frame();
                func = frame.func();
                pc = frame.pc;
                base = frame.base_index(&self.stack);
            }};
        }
        macro_rules! ret {
            ($value:expr) => {{
                self.finish_call($value);
                if self.depth() == stop {
                    return Ok(self.pop());
                }
                resume!();
            }};
        }
        loop {
            let op = func.code[pc];
            pc += 1;
            match op {
                Op::Jump(target) => pc = target as usize,
                Op::Call(index, argc) => {
                    let callee = Rc::as_ptr(&self.g.funcs[index as usize]);
                    self.set_pc(pc);
                    self.push_frame(callee, argc as usize, &[], func.spans[pc - 1])?;
                    resume!();
                }
                Op::CallValue(argc) => {
                    let callee = self.pop();
                    self.set_pc(pc);
                    self.push_value_frame(&callee, argc as usize, func.spans[pc - 1])?;
                    resume!();
                }
                Op::Return => {
                    let v = self.pop();
                    ret!(v);
                }
                op => match self.exec(func, op, pc, base)? {
                    Flow::Next => {}
                    Flow::Jump(target) => pc = target,
                    Flow::Return(v) => ret!(v),
                },
            }
        }
    }

    /// Runs one op of `func`. `pc` is the position after the op, and `base` the
    /// stack index of the current call's first local slot. Calls, returns and
    /// plain jumps are left to the caller.
    #[inline(always)]
    fn exec(&mut self, func: &Func, op: Op, pc: usize, base: usize) -> R<Flow> {
        macro_rules! span {
            () => {
                func.spans[pc - 1]
            };
        }
        match op {
            Op::Int(n) => self.stack.push(Value::Int(n)),
            Op::Const(k) => self.stack.push(func.consts[k as usize].clone()),
            Op::Bool(b) => self.stack.push(Value::Bool(b)),
            Op::Unit => self.stack.push(Value::Unit),
            Op::Load(slot) => {
                let v = self.stack[base + slot as usize].clone();
                self.stack.push(v);
            }
            Op::Store(slot) => {
                let v = self.pop();
                self.stack[base + slot as usize] = v;
            }
            Op::Pop => {
                self.stack.pop();
            }
            Op::Unary(op) => {
                let v = match (op, self.pop()) {
                    (UnOp::Not, Value::Bool(b)) => Value::Bool(!b),
                    (UnOp::Neg, Value::Float(f)) => Value::Float(-f),
                    (UnOp::Neg, Value::Int(n)) => match n.checked_neg() {
                        Some(n) => Value::Int(n),
                        None => return Err(panic("E0503", "integer overflow in negation", span!())),
                    },
                    _ => return Err(internal(span!())),
                };
                self.stack.push(v);
            }
            Op::Arith(op) => {
                let r = self.pop();
                let l = self.pop();
                if let (Value::Int(a), Value::Int(b)) = (&l, &r) {
                    if let Some(v) = int_fast(op, *a, *b) {
                        self.stack.push(Value::Int(v));
                        return Ok(Flow::Next);
                    }
                }
                let v = arith(op, &l, &r, span!())?;
                self.stack.push(v);
            }
            Op::Cmp(op) => {
                let r = self.pop();
                let l = self.pop();
                let v = match (&l, &r) {
                    (Value::Int(a), Value::Int(b)) => ordering_matches(op, a.cmp(b)),
                    _ => compare(op, &l, &r),
                };
                self.stack.push(Value::Bool(v));
            }
            Op::ArithSlotInt(op, slot, n) => {
                let v = match &self.stack[base + slot as usize] {
                    Value::Int(a) => match int_fast(op, *a, n as i64) {
                        Some(v) => Value::Int(v),
                        None => int_arith(op, *a, n as i64, span!())?,
                    },
                    other => arith(op, other, &Value::Int(n as i64), span!())?,
                };
                self.stack.push(v);
            }
            Op::CmpSlotInt(op, slot, n) => {
                let v = match &self.stack[base + slot as usize] {
                    Value::Int(a) => ordering_matches(op, a.cmp(&(n as i64))),
                    other => compare(op, other, &Value::Int(n as i64)),
                };
                self.stack.push(Value::Bool(v));
            }
            Op::ArithInt(op, n) => {
                let v = match self.pop() {
                    Value::Int(a) => match int_fast(op, a, n as i64) {
                        Some(v) => Value::Int(v),
                        None => int_arith(op, a, n as i64, span!())?,
                    },
                    other => arith(op, &other, &Value::Int(n as i64), span!())?,
                };
                self.stack.push(v);
            }
            Op::CmpInt(op, n) => {
                let v = match self.pop() {
                    Value::Int(a) => ordering_matches(op, a.cmp(&(n as i64))),
                    other => compare(op, &other, &Value::Int(n as i64)),
                };
                self.stack.push(Value::Bool(v));
            }
            Op::JumpUnlessSlotInt(op, slot, n, target) => {
                let holds = match &self.stack[base + slot as usize] {
                    Value::Int(a) => ordering_matches(op, a.cmp(&(n as i64))),
                    other => compare(op, other, &Value::Int(n as i64)),
                };
                if !holds {
                    return Ok(Flow::Jump(target as usize));
                }
            }
            Op::JumpUnlessInt(op, n, target) => {
                let holds = match self.pop() {
                    Value::Int(a) => ordering_matches(op, a.cmp(&(n as i64))),
                    other => compare(op, &other, &Value::Int(n as i64)),
                };
                if !holds {
                    return Ok(Flow::Jump(target as usize));
                }
            }
            Op::JumpIfFalse(target) => {
                if !matches!(self.pop(), Value::Bool(true)) {
                    return Ok(Flow::Jump(target as usize));
                }
            }
            Op::AndJump(target) => {
                if matches!(self.stack.last(), Some(Value::Bool(false))) {
                    return Ok(Flow::Jump(target as usize));
                } else {
                    self.stack.pop();
                }
            }
            Op::OrJump(target) => {
                if matches!(self.stack.last(), Some(Value::Bool(true))) {
                    return Ok(Flow::Jump(target as usize));
                } else {
                    self.stack.pop();
                }
            }
            Op::CallBuiltin(name, argc) => {
                let args = self.stack.split_off(self.stack.len() - argc as usize);
                let Value::Str(name) = &func.consts[name as usize] else {
                    return Err(internal(span!()));
                };
                let v = self.builtin(name, args, span!())?;
                self.stack.push(v);
            }
            Op::CallMethod(name, argc) => {
                let args = self.stack.split_off(self.stack.len() - argc as usize);
                let recv = self.pop();
                let Value::Str(name) = &func.consts[name as usize] else {
                    return Err(internal(span!()));
                };
                let v = self.method(recv, name, args, span!())?;
                self.stack.push(v);
            }
            Op::Try(target) => {
                let v = self.pop();
                let inner = match &v {
                    Value::Enum(e) if matches!(&*e.variant, "Some" | "Ok") => Some(e.args[0].clone()),
                    Value::Enum(_) => None,
                    _ => return Err(internal(span!())),
                };
                match inner {
                    Some(inner) => self.stack.push(inner),
                    None if target == NONE => return Ok(Flow::Return(v)),
                    None => {
                        self.stack.push(v);
                        return Ok(Flow::Jump(target as usize));
                    }
                }
            }
            Op::MakeList(n) => {
                let items = self.stack.split_off(self.stack.len() - n as usize);
                self.stack.push(Value::list(items));
            }
            Op::Take(slot) => {
                let v = std::mem::replace(&mut self.stack[base + slot as usize], Value::Unit);
                self.stack.push(v);
            }
            Op::NewListFor(slot) => {
                let room = match &self.stack[base + slot as usize] {
                    Value::List(items) => items.len(),
                    Value::Ints(items) => items.len(),
                    _ => 0,
                };
                self.stack.push(Value::list(Vec::with_capacity(room)));
            }
            Op::MakeMap(n) => {
                let mut map = BTreeMap::new();
                let mut items = self.stack.split_off(self.stack.len() - 2 * n as usize).into_iter();
                while let (Some(k), Some(v)) = (items.next(), items.next()) {
                    map.insert(to_key(&k, span!())?, v);
                }
                self.stack.push(Value::Map(Rc::new(map)));
            }
            Op::MakeStruct(index) => {
                let tmpl = &func.structs[index as usize];
                let given = self.stack.split_off(self.stack.len() - tmpl.argc);
                let fields = tmpl.fields.iter().map(|(name, i)| (name.clone(), given[*i].clone())).collect();
                self.stack.push(Value::Struct(Rc::new(StructVal { name: tmpl.name.clone(), fields })));
            }
            Op::MakeEnum(index, argc) => {
                let args = self.stack.split_off(self.stack.len() - argc as usize);
                let (enum_name, variant) = &func.enums[index as usize];
                self.stack.push(Value::variant(enum_name.clone(), variant.clone(), args));
            }
            Op::MakeClosure(index, n) => {
                let captured = self.stack.split_off(self.stack.len() - n as usize);
                let func = self.g.funcs[index as usize].clone();
                self.stack.push(Value::Closure(Rc::new(ClosureVal { func, captured })));
            }
            Op::FnRef(index) => self.stack.push(Value::Fn(self.g.funcs[index as usize].clone())),
            Op::GetField(name) => {
                let Value::Str(name) = &func.consts[name as usize] else {
                    return Err(internal(span!()));
                };
                let v = match self.pop() {
                    Value::Struct(s) => s.fields.iter().find(|(n, _)| **n == **name).map(|(_, v)| v.clone()),
                    _ => None,
                };
                self.stack.push(v.ok_or_else(|| internal(span!()))?);
            }
            Op::Index => {
                let index = self.pop();
                let v = match (self.pop(), index) {
                    (Value::List(items), Value::Int(i)) => {
                        if i < 0 || i as usize >= items.len() {
                            return Err(out_of_bounds(i, items.len(), span!()));
                        }
                        items[i as usize].clone()
                    }
                    (Value::Ints(items), Value::Int(i)) => {
                        if i < 0 || i as usize >= items.len() {
                            return Err(out_of_bounds(i, items.len(), span!()));
                        }
                        Value::Int(items[i as usize])
                    }
                    (Value::Map(entries), index) => {
                        let key = to_key(&index, span!())?;
                        entries.get(&key).cloned().ok_or_else(|| missing_key(&key, span!()))?
                    }
                    _ => return Err(internal(span!())),
                };
                self.stack.push(v);
            }
            Op::Concat(n) => {
                let start = self.stack.len() - n as usize;
                let mut out = String::new();
                for v in &self.stack[start..] {
                    match v {
                        Value::Str(s) => out.push_str(s),
                        other => out.push_str(&other.repr()),
                    }
                }
                self.stack.truncate(start);
                self.stack.push(Value::str(out));
            }
            Op::StoreIndex(slot) => {
                let index = self.pop();
                let v = self.pop();
                store_index(&mut self.stack[base + slot as usize], index, v, span!())?;
            }
            Op::StoreField(slot, name) => {
                let v = self.pop();
                let Value::Str(name) = &func.consts[name as usize] else {
                    return Err(internal(span!()));
                };
                *field_mut(&mut self.stack[base + slot as usize], name, NONE, span!())? = v;
            }
            Op::StoreFieldAt(slot, at) => {
                let v = self.pop();
                *field_mut(&mut self.stack[base + slot as usize], "", at, span!())? = v;
            }
            Op::GetFieldAt(at) => {
                let v = match self.pop() {
                    Value::Struct(s) => s.fields.get(at as usize).map(|(_, v)| v.clone()),
                    _ => None,
                };
                self.stack.push(v.ok_or_else(|| internal(span!()))?);
            }
            Op::Assign(slot, path) => {
                let span = span!();
                let path = &func.paths[path as usize];
                let Some((last, init)) = path.split_last() else {
                    return Err(internal(span));
                };
                // Indexes were pushed outermost first, so they pop from the root down.
                let count = path.iter().filter(|e| matches!(e, PathElem::Index(_))).count();
                let mut indexes = Vec::with_capacity(count);
                for _ in 0..count {
                    indexes.push(self.pop());
                }
                let mut indexes = indexes.into_iter();
                let v = self.pop();
                let mut cur = &mut self.stack[base + slot as usize];
                for elem in init {
                    cur = match elem {
                        PathElem::Field(name, span, field) => field_mut(cur, name, *field, *span)?,
                        PathElem::Index(at) => {
                            let index = indexes.next().ok_or_else(|| internal(span))?;
                            element_mut(cur, index, *at)?
                        }
                    };
                }
                match last {
                    PathElem::Field(name, span, field) => *field_mut(cur, name, *field, *span)? = v,
                    PathElem::Index(at) => {
                        let index = indexes.next().ok_or_else(|| internal(span))?;
                        store_index(cur, index, v, *at)?;
                    }
                }
            }
            Op::InPlace(slot, path, kind) => {
                let span = span!();
                // `first` is the pushed value or the key; `second` is the value for `set`.
                let second = if kind == InPlace::Set { self.pop() } else { Value::Unit };
                let first = self.pop();
                let path: &[PathElem] = if path == NONE { &[] } else { &func.paths[path as usize] };
                let place = fields_mut(&mut self.stack[base + slot as usize], path, span)?;
                if kind == InPlace::Push {
                    if let Value::Int(n) = first {
                        match place {
                            Value::Ints(items) => {
                                Rc::make_mut(items).push(n);
                                return Ok(Flow::Next);
                            }
                            // The first Int pushed onto an empty list decides its representation.
                            Value::List(items) if items.is_empty() => {
                                let mut ints = Vec::with_capacity(items.capacity().max(4));
                                ints.push(n);
                                *place = Value::Ints(Rc::new(ints));
                                return Ok(Flow::Next);
                            }
                            _ => {}
                        }
                    } else if let Value::Ints(ints) = place {
                        let items = unpack(ints);
                        *place = Value::List(Rc::new(items));
                    }
                }
                match (place, kind) {
                    (Value::List(items), InPlace::Push) => Rc::make_mut(items).push(first),
                    (Value::Map(entries), InPlace::Set) => {
                        let key = to_key(&first, span)?;
                        Rc::make_mut(entries).insert(key, second);
                    }
                    (Value::Map(entries), InPlace::Remove) => {
                        let key = to_key(&first, span)?;
                        Rc::make_mut(entries).remove(&key);
                    }
                    (other, _) => {
                        let recv = other.clone();
                        let args = if kind == InPlace::Set { vec![first, second] } else { vec![first] };
                        let v = self.method(recv, kind.method(), args, span)?;
                        *fields_mut(&mut self.stack[base + slot as usize], path, span)? = v;
                    }
                }
            }
            Op::ForRange(slot, target) => {
                let slot = base + slot as usize;
                match (&self.stack[slot], &self.stack[slot + 1]) {
                    (Value::Int(a), Value::Int(b)) => {
                        if a >= b {
                            return Ok(Flow::Jump(target as usize));
                        }
                    }
                    _ => return Err(internal(span!())),
                }
            }
            Op::ForRangeNext(slot, target) => {
                let slot = base + slot as usize;
                let end = match &self.stack[slot + 1] {
                    Value::Int(end) => *end,
                    _ => return Err(internal(span!())),
                };
                if let Value::Int(i) = &mut self.stack[slot] {
                    *i += 1;
                    if *i < end {
                        return Ok(Flow::Jump(target as usize));
                    }
                }
            }
            Op::ForList(slot, target) => {
                let slot = base + slot as usize;
                let item = match (&self.stack[slot], &self.stack[slot + 1]) {
                    (Value::List(items), Value::Int(i)) => items.get(*i as usize).cloned(),
                    (Value::Ints(items), Value::Int(i)) => items.get(*i as usize).map(|n| Value::Int(*n)),
                    _ => return Err(internal(span!())),
                };
                match item {
                    Some(item) => {
                        self.stack[slot + 2] = item;
                        if let Value::Int(i) = &mut self.stack[slot + 1] {
                            *i += 1;
                        }
                    }
                    None => return Ok(Flow::Jump(target as usize)),
                }
            }
            Op::SaveSp(slot) => {
                self.stack[base + slot as usize] = Value::Int(self.stack.len() as i64);
            }
            Op::RestoreSp(slot) => {
                if let Value::Int(height) = self.stack[base + slot as usize] {
                    self.stack.truncate(height as usize);
                }
            }
            Op::IsVariant(slot, name, argc) => {
                let Value::Str(name) = &func.consts[name as usize] else {
                    return Err(internal(span!()));
                };
                let is = matches!(
                    &self.stack[base + slot as usize],
                    Value::Enum(e) if *e.variant == **name && e.args.len() == argc as usize
                );
                self.stack.push(Value::Bool(is));
            }
            Op::EnumArg(slot, i) => {
                let v = match &self.stack[base + slot as usize] {
                    Value::Enum(e) => e.args.get(i as usize).cloned(),
                    _ => None,
                };
                self.stack.push(v.ok_or_else(|| internal(span!()))?);
            }
            Op::Assert(aux) => {
                if self.pop() != Value::Bool(true) {
                    let msg = format!("assertion failed: `{}`", self.text(func.aux[aux as usize]));
                    return Err(panic("E0501", msg, span!()));
                }
            }
            Op::AssertCmp(op, aux) => {
                let r = self.pop();
                let l = self.pop();
                if !compare(op, &l, &r) {
                    let msg = format!(
                        "assertion failed: `{}` (left: {}, right: {})",
                        self.text(func.aux[aux as usize]),
                        l.repr(),
                        r.repr()
                    );
                    return Err(panic("E0501", msg, span!()));
                }
            }
            Op::Require(aux) => {
                if self.pop() != Value::Bool(true) {
                    let msg = format!(
                        "precondition of `{}` violated: `{}`",
                        short(&func.name),
                        self.text(func.aux[aux as usize])
                    );
                    return Err(panic("E0502", msg, self.frame().call_span()));
                }
            }
            Op::Ensure(aux, slot) => {
                if self.pop() != Value::Bool(true) {
                    let msg = format!(
                        "postcondition of `{}` violated: `{}` (result: {})",
                        short(&func.name),
                        self.text(func.aux[aux as usize]),
                        self.stack[base + slot as usize].repr()
                    );
                    return Err(panic("E0502", msg, self.frame().call_span()));
                }
            }
            Op::Internal => return Err(internal(span!())),
            Op::Jump(_) | Op::Call(..) | Op::CallValue(_) | Op::Return => return Err(internal(span!())),
        }
        Ok(Flow::Next)
    }

    fn builtin(&mut self, name: &str, mut args: Vec<Value>, span: Span) -> R<Value> {
        let text = |v: Option<Value>| v.map(|v| v.display()).unwrap_or_default();
        let result = |r: Result<Value, String>| match r {
            Ok(v) => Value::ok(v),
            Err(e) => Value::err(Value::str(e)),
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
            // The window, input, the clock and random numbers (see `window.rs`).
            _ => {
                use crate::window;
                let int = |i: usize| match args.get(i) {
                    Some(Value::Int(n)) => *n,
                    _ => 0,
                };
                let float = |i: usize| match args.get(i) {
                    Some(Value::Float(f)) => *f,
                    _ => 0.0,
                };
                let name_of = |i: usize| args.get(i).map(|v| v.display()).unwrap_or_default();
                Ok(match name {
                    "window_open" => match window::open(&name_of(0), int(1), int(2), int(3)) {
                        Ok(()) => Value::Unit,
                        Err(why) => return Err(panic("E0510", why, span)),
                    },
                    "window_update" => Value::Bool(window::update()),
                    "window_close" => {
                        window::close();
                        Value::Unit
                    }
                    "draw_clear" => {
                        window::clear(int(0));
                        Value::Unit
                    }
                    "draw_pixel" => {
                        window::pixel(int(0), int(1), int(2));
                        Value::Unit
                    }
                    "draw_rect" => {
                        window::rect(int(0), int(1), int(2), int(3), int(4));
                        Value::Unit
                    }
                    "rgb" => Value::Int(window::rgb(int(0), int(1), int(2))),
                    "key_down" => Value::Bool(window::key_down(&name_of(0))),
                    "mouse_x" => Value::Int(window::mouse(0)),
                    "mouse_y" => Value::Int(window::mouse(1)),
                    "mouse_down" => Value::Bool(window::mouse_down(int(0))),
                    "time_ms" => Value::Int(window::time_ms()),
                    "sleep_ms" => {
                        window::sleep_ms(int(0));
                        Value::Unit
                    }
                    "random" => Value::Int(window::random(int(0))),
                    "random_seed" => {
                        window::random_seed(int(0));
                        Value::Unit
                    }
                    "gpu_open" => match window::gpu_open(&name_of(0), int(1), int(2)) {
                        Ok(()) => Value::Unit,
                        Err(why) => return Err(panic("E0510", why, span)),
                    },
                    "gpu_clear" => {
                        window::gpu_clear(int(0));
                        Value::Unit
                    }
                    "gpu_camera" => {
                        window::gpu_camera(float(0), float(1), float(2), float(3), float(4));
                        Value::Unit
                    }
                    "gpu_fog" => {
                        window::gpu_fog(int(0), int(1), int(2));
                        Value::Unit
                    }
                    "gpu_mesh" => {
                        let (mut points, mut colors) = (Vec::new(), Vec::new());
                        if let Some(Value::List(ps)) = args.first() {
                            points.extend(ps.iter().map(|v| if let Value::Float(f) = v { *f } else { 0.0 }));
                        }
                        match args.get(1) {
                            Some(Value::Ints(cs)) => colors.extend(cs.iter()),
                            Some(Value::List(cs)) => {
                                colors.extend(cs.iter().map(|v| if let Value::Int(n) = v { *n } else { 0 }))
                            }
                            _ => {}
                        }
                        Value::Int(window::gpu_mesh(&points, &colors))
                    }
                    "gpu_draw" => {
                        window::gpu_draw(int(0), float(1), float(2), float(3));
                        Value::Unit
                    }
                    "gpu_free" => {
                        window::gpu_free(int(0));
                        Value::Unit
                    }
                    "gpu_rect" => {
                        window::gpu_rect(int(0), int(1), int(2), int(3), int(4));
                        Value::Unit
                    }
                    "gpu_text" => {
                        window::gpu_text(int(0), int(1), int(2), int(3), &name_of(4));
                        Value::Unit
                    }
                    "text_input" => Value::str(&window::text_input()),
                    "text_cursor" => {
                        window::text_cursor(int(0), int(1), int(2));
                        Value::Unit
                    }
                    "gpu_text_width" => Value::Int(window::gpu_text_width(int(0), &name_of(1))),
                    "env" => Value::str(&std::env::var(name_of(0)).unwrap_or_default()),
                    "http_start" => {
                        let headers: Vec<String> = match args.get(2) {
                            Some(Value::List(items)) => items.iter().map(|v| v.display()).collect(),
                            _ => Vec::new(),
                        };
                        Value::Int(crate::net::start(&name_of(0), &name_of(1), &headers, &name_of(3)))
                    }
                    "http_done" => Value::Bool(crate::net::done(int(0))),
                    "http_result" => result(crate::net::take(int(0)).map(|text| Value::str(&text))),
                    _ => return Err(internal(span)),
                })
            }
        }
    }

    fn method(&mut self, recv: Value, name: &str, args: Vec<Value>, span: Span) -> R<Value> {
        // The common methods work on a packed list directly.
        if let Value::Ints(items) = &recv {
            let int = |n: Option<&i64>| Value::option(n.map(|n| Value::Int(*n)));
            match (name, args.as_slice()) {
                ("len", []) => return Ok(Value::Int(items.len() as i64)),
                ("is_empty", []) => return Ok(Value::Bool(items.is_empty())),
                ("push", [Value::Int(n)]) => {
                    let mut out = Vec::with_capacity(items.len() + 1);
                    out.extend_from_slice(items);
                    out.push(*n);
                    return Ok(Value::Ints(Rc::new(out)));
                }
                ("get", [Value::Int(i)]) => return Ok(int(usize::try_from(*i).ok().and_then(|i| items.get(i)))),
                ("first", []) => return Ok(int(items.first())),
                ("last", []) => return Ok(int(items.last())),
                ("contains", [Value::Int(n)]) => return Ok(Value::Bool(items.contains(n))),
                ("reverse", []) => return Ok(Value::Ints(Rc::new(items.iter().rev().copied().collect()))),
                ("sort", []) => {
                    let mut out = items.to_vec();
                    out.sort_unstable();
                    return Ok(Value::Ints(Rc::new(out)));
                }
                ("slice", [start, end]) => {
                    let (a, b) = slice_range(start, end, items.len(), span)?;
                    return Ok(Value::Ints(Rc::new(items[a..b].to_vec())));
                }
                _ => {}
            }
        }
        // Everything else works on ordinary lists.
        let recv = unpacked(recv);
        let args: Vec<Value> = args.into_iter().map(unpacked).collect();
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
                // Floats in the order of `f64::total_cmp`, so that the result is
                // settled whatever the list holds: -0.0 before 0.0, NaN last.
                items.sort_by(|a, b| match (a, b) {
                    (Value::Float(a), Value::Float(b)) => a.total_cmp(b),
                    _ => order(a, b).unwrap_or(Ordering::Equal),
                });
                Ok(Value::list(items))
            }
            (Value::List(items), "join", [Value::Str(sep)]) => {
                let parts: Vec<String> = items.iter().map(|v| v.display()).collect();
                Ok(Value::str(parts.join(sep)))
            }
            (Value::List(items), "map", [f]) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items.iter() {
                    out.push(self.call_value(f, [item.clone()], span)?);
                }
                Ok(Value::list(out))
            }
            (Value::List(items), "filter", [f]) => {
                let mut out = Vec::new();
                for item in items.iter() {
                    if truthy(self.call_value(f, [item.clone()], span)?) {
                        out.push(item.clone());
                    }
                }
                Ok(Value::list(out))
            }
            (Value::List(items), "any", [f]) => {
                for item in items.iter() {
                    if truthy(self.call_value(f, [item.clone()], span)?) {
                        return Ok(Value::Bool(true));
                    }
                }
                Ok(Value::Bool(false))
            }
            (Value::List(items), "all", [f]) => {
                for item in items.iter() {
                    if !truthy(self.call_value(f, [item.clone()], span)?) {
                        return Ok(Value::Bool(false));
                    }
                }
                Ok(Value::Bool(true))
            }
            (Value::List(items), "fold", [init, f]) => {
                let mut acc = init.clone();
                for item in items.iter() {
                    acc = self.call_value(f, [acc, item.clone()], span)?;
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

            (Value::Str(s), "codes", []) => Ok(Value::list(s.chars().map(|c| Value::Int(c as i64)).collect())),
            (Value::Int(a), "bit_and", [Value::Int(b)]) => Ok(Value::Int(a & b)),
            (Value::Int(a), "bit_or", [Value::Int(b)]) => Ok(Value::Int(a | b)),
            (Value::Int(a), "bit_xor", [Value::Int(b)]) => Ok(Value::Int(a ^ b)),
            (Value::Int(a), "shl" | "shr", [Value::Int(b)]) => {
                if !(0..64).contains(b) {
                    return Err(panic_hint(
                        "E0509",
                        format!("shift amount {b} is out of range"),
                        span,
                        "the amount must be between 0 and 63",
                    ));
                }
                // Bits shifted out are lost; `shr` keeps the sign.
                Ok(Value::Int(if name == "shl" { a.wrapping_shl(*b as u32) } else { a >> b }))
            }
            (Value::Int(n), "to_char", []) => match u32::try_from(*n).ok().and_then(char::from_u32) {
                Some(c) => Ok(Value::str(c.to_string())),
                None => Err(panic("E0509", format!("{n} is not a character code"), span)),
            },
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

            (Value::Enum(e), "unwrap_or", [default]) => Ok(match &*e.variant {
                "Some" | "Ok" => e.args[0].clone(),
                _ => default.clone(),
            }),
            (Value::Enum(e), "is_some", []) => Ok(Value::Bool(&*e.variant == "Some")),
            (Value::Enum(e), "is_none", []) => Ok(Value::Bool(&*e.variant == "None")),
            (Value::Enum(e), "is_ok", []) => Ok(Value::Bool(&*e.variant == "Ok")),
            (Value::Enum(e), "is_err", []) => Ok(Value::Bool(&*e.variant == "Err")),
            _ => Err(internal(span)),
        }
    }
}

// ---- generated code ----

#[cfg(not(target_arch = "x86_64"))]
impl Interp<'_> {
    fn native_available(&mut self) -> bool {
        false
    }

    fn enter_native(&mut self) -> R<Value> {
        Err(internal(Span::new(0, 0)))
    }
}

/// The interpreter's side of generated machine code: entering it, and the
/// helpers it calls back.
#[cfg(target_arch = "x86_64")]
mod native {
    use super::*;
    use crate::jit;

    type Entry = unsafe extern "sysv64" fn(*mut Stack) -> u32;

    /// The interpreter a stack belongs to.
    ///
    /// Safety: `stack` must be the stack of an interpreter that is inside
    /// `enter_native`, which is where `owner` is set.
    unsafe fn owner<'i>(stack: *mut Stack) -> &'i mut Interp<'static> {
        &mut *((*stack).owner as *mut Interp<'static>)
    }

    unsafe extern "sysv64" fn op(stack: *mut Stack, pc: u32) -> u32 {
        owner(stack).native_op(pc as usize)
    }

    unsafe extern "sysv64" fn call(stack: *mut Stack, index: u32, argc: u32, pc: u32) -> usize {
        owner(stack).native_call(index as usize, argc as usize, pc as usize)
    }

    unsafe extern "sysv64" fn call_value(stack: *mut Stack, argc: u32, pc: u32) -> usize {
        owner(stack).native_call_value(argc as usize, pc as usize)
    }

    unsafe extern "sysv64" fn ret(stack: *mut Stack) {
        let interp = owner(stack);
        let value = interp.pop();
        interp.finish_call(value);
    }

    fn helpers() -> jit::Helpers {
        let (vec_ptr, vec_len, vec_cap) = vec_layout().unwrap_or((0, 8, 16));
        jit::Helpers {
            op: op as *const () as usize,
            call: call as *const () as usize,
            call_value: call_value as *const () as usize,
            ret: ret as *const () as usize,
            vec_ptr,
            vec_len,
            vec_cap,
            closure_func: std::mem::offset_of!(ClosureVal, func) as i32,
            closure_captured: std::mem::offset_of!(ClosureVal, captured) as i32,
            struct_fields: std::mem::offset_of!(StructVal, fields) as i32,
            field_size: std::mem::size_of::<(Rc<str>, Value)>() as i32,
            field_value: field_value_offset() as i32,
        }
    }

    /// Where the value sits inside one `(name, value)` entry of a struct.
    fn field_value_offset() -> usize {
        let entry: (Rc<str>, Value) = (Rc::from(""), Value::Unit);
        &entry.1 as *const Value as usize - &entry as *const (Rc<str>, Value) as usize
    }

    /// Generated code compares strings itself, and reads struct fields.
    fn string_and_struct_layout_matches() -> bool {
        let word = |v: &Value, at: usize| unsafe { *((v as *const Value as *const u8).add(at) as *const usize) };
        let text: Rc<str> = Rc::from("abc");
        let string = Value::Str(text.clone());
        // A string is its counts pointer and its length; the bytes follow the counts.
        let bytes_follow_counts = Rc::as_ptr(&text) as *const u8 as usize == word(&string, 8) + jit::RC_DATA as usize;
        let mut fields: Vec<(Rc<str>, Value)> = Vec::with_capacity(7);
        fields.extend([(text.clone(), Value::Unit), (text.clone(), Value::Unit), (text.clone(), Value::Unit)]);
        // Safety: a Vec is three words.
        let words: [usize; 3] = unsafe { std::mem::transmute_copy(&fields) };
        let find = |wanted: usize| words.iter().position(|w| *w == wanted).map(|i| i as i32 * 8);
        let fields_layout = (|| Some((find(fields.as_ptr() as usize)?, find(3)?, find(7)?)))();
        byte_at(&string, 0) == jit::TAG_STR
            && word(&string, 16) == 3
            && bytes_follow_counts
            && std::mem::size_of::<Vec<(Rc<str>, Value)>>() == 24
            && fields_layout == vec_layout()
            && byte_at(&Value::Struct(Rc::new(StructVal { name: text.clone(), fields: Vec::new() })), 0) == jit::TAG_STRUCT
    }

    fn byte_at(v: &Value, at: usize) -> u8 {
        // Safety: reads one byte inside the value.
        unsafe { *(v as *const Value as *const u8).add(at) }
    }

    /// Whether calls to `func` may reuse earlier results. That is sound for a
    /// function without effects: nothing but its arguments can influence it.
    /// It pays off when the function calls itself more than once per call,
    /// which is where the same arguments come up again and again.
    fn memoizable(func: &Func, funcs: &[Rc<Func>]) -> bool {
        let n = func.nparams;
        let calls_itself = |op: &Op| match op {
            Op::Call(index, _) => funcs.get(*index as usize).is_some_and(|f| std::ptr::eq(Rc::as_ptr(f), func)),
            _ => false,
        };
        func.pure_fn
            && func.int_ret
            && (1..=3).contains(&n)
            && (0..n).all(|s| func.int_slots.get(s).copied().unwrap_or(false))
            && func.nlocals <= jit::MAX_INLINE_LOCALS
            && func.code.iter().filter(|op| calls_itself(op)).count() >= 2
    }

    /// Offsets of the pointer, length and capacity inside a `Vec<Value>`.
    /// Rust does not promise an order, so it is measured on a sample.
    fn vec_layout() -> Option<(i32, i32, i32)> {
        if std::mem::size_of::<Vec<Value>>() != 24 {
            return None;
        }
        let mut sample: Vec<Value> = Vec::with_capacity(7);
        sample.extend([Value::Unit, Value::Unit, Value::Unit]);
        // Safety: a Vec is three words, as checked above.
        let words: [usize; 3] = unsafe { std::mem::transmute_copy(&sample) };
        let find = |wanted: usize| words.iter().position(|w| *w == wanted).map(|i| i as i32 * 8);
        Some((find(sample.as_ptr() as usize)?, find(sample.len())?, find(sample.capacity())?))
    }

    /// The same measurement for `Vec<i64>`, which packed lists use.
    fn int_vec_layout() -> Option<(i32, i32, i32)> {
        if std::mem::size_of::<Vec<i64>>() != 24 {
            return None;
        }
        let mut sample: Vec<i64> = Vec::with_capacity(7);
        sample.extend([0, 0, 0]);
        // Safety: a Vec is three words, as checked above.
        let words: [usize; 3] = unsafe { std::mem::transmute_copy(&sample) };
        let find = |wanted: usize| words.iter().position(|w| *w == wanted).map(|i| i as i32 * 8);
        Some((find(sample.as_ptr() as usize)?, find(sample.len())?, find(sample.capacity())?))
    }

    /// Generated code clones and drops heap values by adjusting the strong
    /// count their second word points at. Checks that on one value of each kind.
    fn heap_values_are_counted() -> bool {
        let word = |v: &Value| unsafe { *((v as *const Value as *const u8).add(8) as *const usize) };
        // `counts` is where the value's reference counts should be.
        let check = |v: Value, counts: usize, strong: usize| {
            // Safety: `counts` is the start of a live reference-counted block.
            word(&v) == counts && unsafe { *(counts as *const usize) } == strong + 1
        };
        let counts_of = |data: *const u8| data as usize - jit::RC_DATA as usize;
        let text: Rc<str> = Rc::from("x");
        let list = Rc::new(Vec::<Value>::new());
        let ints = Rc::new(vec![7i64]);
        let byte = |v: &Value, at: usize| unsafe { *(v as *const Value as *const u8).add(at) };
        let map = Rc::new(BTreeMap::<Key, Value>::new());
        let record = Rc::new(StructVal { name: text.clone(), fields: Vec::new() });
        let choice = Rc::new(EnumVal { enum_name: text.clone(), variant: text.clone(), args: Vec::new() });
        let func = Rc::new(Func::default());
        let closure = Rc::new(ClosureVal { func: func.clone(), captured: Vec::new() });
        // Safety: reads the `func` field of a live closure.
        let closure_func = unsafe { *(Rc::as_ptr(&closure) as *const usize) };
        closure_func == counts_of(Rc::as_ptr(&func) as *const u8)
            && std::mem::offset_of!(ClosureVal, func) == 0
            && check(Value::Str(text.clone()), counts_of(Rc::as_ptr(&text) as *const u8), 4)
            && check(Value::List(list.clone()), counts_of(Rc::as_ptr(&list) as *const u8), 1)
            && check(Value::Ints(ints.clone()), counts_of(Rc::as_ptr(&ints) as *const u8), 1)
            && byte(&Value::Ints(ints.clone()), 0) == jit::TAG_INTS
            && int_vec_layout() == vec_layout()
            && check(Value::Map(map.clone()), counts_of(Rc::as_ptr(&map) as *const u8), 1)
            && check(Value::Struct(record.clone()), counts_of(Rc::as_ptr(&record) as *const u8), 1)
            && check(Value::Enum(choice.clone()), counts_of(Rc::as_ptr(&choice) as *const u8), 1)
            && check(Value::Fn(func.clone()), counts_of(Rc::as_ptr(&func) as *const u8), 2)
            && check(Value::Closure(closure.clone()), counts_of(Rc::as_ptr(&closure) as *const u8), 1)
    }

    /// Generated code relies on how `Value` and `Stack` are laid out.
    fn layout_matches() -> bool {
        let byte = |v: &Value, at: usize| unsafe { *(v as *const Value as *const u8).add(at) };
        let int = Value::Int(0x0102_0304_0506_0708);
        let number = unsafe { *((&int as *const Value as *const u8).add(8) as *const i64) };
        let truth = Value::Bool(true);
        std::mem::size_of::<Value>() == jit::VALUE_SIZE
            && byte(&int, 0) == jit::TAG_INT
            && number == 0x0102_0304_0506_0708
            && byte(&truth, 0) == jit::TAG_BOOL
            && byte(&truth, 1) == 1
            && byte(&Value::Bool(false), 1) == 0
            && byte(&Value::Unit, 0) == jit::TAG_UNIT
            && byte(&Value::Float(0.0), 0) < jit::FIRST_HEAP_TAG
            && byte(&Value::str(""), 0) == jit::FIRST_HEAP_TAG
            && byte(&Value::list(Vec::new()), 0) == jit::TAG_LIST
            && vec_layout().is_some()
            && heap_values_are_counted()
            && string_and_struct_layout_matches()
            && std::mem::offset_of!(Stack, top) == jit::STACK_TOP_OFFSET as usize
            && std::mem::offset_of!(Stack, end) == jit::STACK_END_OFFSET as usize
            && std::mem::offset_of!(Stack, frames_top) == jit::FRAMES_TOP_OFFSET as usize
            && std::mem::offset_of!(Stack, frames_limit) == jit::FRAMES_LIMIT_OFFSET as usize
            && std::mem::size_of::<Frame>() == jit::FRAME_SIZE as usize
            && std::mem::offset_of!(Frame, func) == jit::FRAME_FUNC as usize
            && std::mem::offset_of!(Frame, pc) == jit::FRAME_PC as usize
            && std::mem::offset_of!(Frame, base) == jit::FRAME_BASE as usize
            && std::mem::offset_of!(Frame, span_start) == jit::FRAME_SPAN as usize
            && std::mem::offset_of!(Frame, span_end) == jit::FRAME_SPAN as usize + 8
    }

    impl Interp<'_> {
        /// True when machine code can be generated and run here.
        pub(super) fn native_available(&mut self) -> bool {
            layout_matches() && self.code.install(&[0xC3]).is_some()
        }

        /// Address of the machine code for the top frame's function.
        fn entry(&mut self) -> R<usize> {
            let frame = self.frame();
            let func = frame.func();
            let address = func.native.get();
            if address != 0 {
                return Ok(address);
            }
            let span = frame.call_span();
            func.need.set(func.nlocals + func.code.len() + 64);
            if func.memo.get() == 0 && memoizable(func, &self.g.funcs) {
                let table = vec![0u64; jit::MEMO_ENTRIES * (func.nparams + 2)].into_boxed_slice();
                func.memo.set(table.as_ptr() as usize);
                self.memo_tables.push(table);
            }
            let funcs = &self.g.funcs;
            let callee = |index: u32| {
                funcs.get(index as usize).map(|f| jit::Callee {
                    func: Rc::as_ptr(f) as usize,
                    native: f.native.as_ptr() as usize,
                    nlocals: f.nlocals,
                    int_ret: f.int_ret,
                    // The same margin `push_frame` asks for.
                    need: f.nlocals + f.code.len() + 64,
                })
            };
            let code = jit::compile(func, &helpers(), &callee);
            match self.code.install(&code) {
                Some(address) => {
                    func.native.set(address);
                    Ok(address)
                }
                None => Err(panic("E0599", "internal error: cannot allocate executable memory", span)),
            }
        }

        /// Runs the frame that was just pushed as machine code and returns its result.
        pub(super) fn enter_native(&mut self) -> R<Value> {
            let entry = self.entry()?;
            self.stack.owner = self as *mut Interp<'_> as *mut u8;
            // Safety: `entry` is the address of code generated for the top
            // frame's function, which follows the `Entry` calling convention.
            let status = unsafe { std::mem::transmute::<usize, Entry>(entry)(&mut self.stack) };
            if status != 0 {
                return Err(self.fault.take().unwrap_or_else(|| internal(Span::new(0, 0))));
            }
            Ok(self.pop())
        }

        fn native_op(&mut self, pc: usize) -> u32 {
            let frame = self.frame();
            let base = frame.base_index(&self.stack);
            let func = frame.func();
            match self.exec(func, func.code[pc], pc + 1, base) {
                Ok(Flow::Next) => jit::NEXT,
                Ok(Flow::Jump(_)) => jit::JUMP,
                Ok(Flow::Return(value)) => {
                    self.finish_call(value);
                    jit::RETURNED
                }
                Err(fault) => {
                    self.fault = Some(fault);
                    jit::FAILED
                }
            }
        }

        /// Starts a call and returns the callee's entry address, or 0 on failure.
        fn native_call(&mut self, index: usize, argc: usize, pc: usize) -> usize {
            let span = self.frame().func().spans[pc];
            let callee = Rc::as_ptr(&self.g.funcs[index]);
            let entry = self.push_frame(callee, argc, &[], span).and_then(|_| self.entry());
            self.entry_or_fault(entry)
        }

        fn native_call_value(&mut self, argc: usize, pc: usize) -> usize {
            let span = self.frame().func().spans[pc];
            let callee = self.pop();
            let entry = self.push_value_frame(&callee, argc, span).and_then(|_| self.entry());
            self.entry_or_fault(entry)
        }

        fn entry_or_fault(&mut self, entry: R<usize>) -> usize {
            match entry {
                Ok(address) => address,
                Err(fault) => {
                    self.fault = Some(fault);
                    0
                }
            }
        }
    }
}
