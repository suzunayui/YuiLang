//! Ahead-of-time compiler: turns a program into a Windows x64 executable that
//! contains no interpreter (see `docs/native-backend.md`).
//!
//! It handles programs made of functions over `Int`, `Bool`, `String` and
//! `List<Int>`: arithmetic, comparisons, `if`, `while`, `for`, calls, list
//! literals, `push`, `len`, indexing, `map` / `filter` / `fold` / `any` /
//! `all` with a closure written in place, strings with `{...}` parts,
//! comparisons and a few methods on strings, and `print`. Anything else is
//! reported as `Unsupported`.
//!
//! Lists and strings live on the heap with a reference count in front (the
//! layouts are in `runtime.yui`, which also holds their routines; they are
//! compiled by this same compiler). Every such variable owns one reference,
//! and every such expression produces one; a reference is given up when its
//! variable goes out of scope or is assigned to. String literals are
//! constants in the executable, marked by a negative count that is never
//! changed.
//!
//! Values have no tags: an Int is 8 bytes in a register. Each function keeps
//! its busiest variables in registers that calls preserve (rbx, rsi, rdi,
//! r12-r15) and the rest in its stack frame. Expressions are evaluated into
//! r8-r11; rax, rcx and rdx are scratch. Functions follow the Windows x64
//! calling convention, so they can call the operating system directly.

use super::{condition, Asm, CC_AE, CC_E, CC_GE, CC_L, CC_NE, CC_O, M, R10, R11, R12, R13, R14, R15, R8, R9, RAX, RBX, RCX, RDI, RDX, RSI};
use crate::ast::*;
use crate::diag::{render_human, Diagnostic, SourceMap, Span};
use std::collections::HashMap;

const RSP: u8 = 4;
const CC_NS: u8 = 0x9;
const CC_Z: u8 = CC_E;
const CC_NZ: u8 = CC_NE;

/// Registers for expression values. Calls do not preserve them.
const TEMPS: [u8; 4] = [R8, R9, R10, R11];
/// Registers for variables. Calls preserve them.
const SAVED: [u8; 7] = [RBX, RSI, RDI, R12, R13, R14, R15];
/// The temp each argument is evaluated into, in an order that leaves the
/// later ones free while the earlier ones are computed.
const ARG_TEMPS: [u8; 4] = [R10, R11, R8, R9];
const ARG_REGS: [u8; 4] = [RCX, RDX, R8, R9];

const STDOUT: i64 = -11;
const STDERR: i64 = -12;

/// Functions imported from kernel32.dll, in the order of the import table.
const IMPORTS: [&str; 6] = ["GetStdHandle", "WriteFile", "ExitProcess", "HeapAlloc", "HeapFree", "HeapReAlloc"];
const GET_STD_HANDLE: usize = 0;
const WRITE_FILE: usize = 1;
const EXIT_PROCESS: usize = 2;
const HEAP_ALLOC: usize = 3;
const HEAP_FREE: usize = 4;
const HEAP_REALLOC: usize = 5;

/// The list routines, written in YuiLang.
const RUNTIME: &str = include_str!("runtime.yui");

/// Offset of an element from the start of a list (see `runtime.yui`).
const ELEMENTS: i32 = 24;
const LENGTH: i32 = 8;
const CAPACITY: i32 = 16;

/// What a storage place decided before code generation is for.
const VAR: u8 = 0;
/// The end of a `for` range.
const LIMIT: u8 = 1;
/// The list a loop walks, the position in it, and the result being built.
const LIST: u8 = 2;
const POS: u8 = 3;
const OUT: u8 = 4;

/// Writable memory (zero at start): the number of heap blocks in use, kept
/// only when the executable is built to check for leaks.
const LIVE_BLOCKS: usize = 0;

/// Why a program cannot be compiled natively yet.
pub struct Unsupported {
    pub what: String,
    pub span: Span,
}

type R<T> = Result<T, Unsupported>;

fn unsupported<T>(what: impl Into<String>, span: Span) -> R<T> {
    Err(Unsupported { what: what.into(), span })
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Ty {
    Int,
    Bool,
    Unit,
    /// `List<Int>`: a pointer to a counted block on the heap.
    List,
    /// `String`: a pointer to a counted block on the heap, or to a constant.
    Str,
    /// The expression does not produce a value (`return`, `break`, ...).
    Never,
}

impl Ty {
    /// Whether values of this type are counted references.
    fn heap(self) -> bool {
        matches!(self, Ty::List | Ty::Str)
    }

    /// The routine of `runtime.yui` that gives up a reference.
    fn release(self) -> &'static str {
        match self {
            Ty::Str => "str_release",
            _ => "list_release",
        }
    }
}

#[derive(Clone, Copy)]
enum Loc {
    Reg(u8),
    Stack(i32),
}

#[derive(Clone, Copy)]
struct Local {
    ty: Ty,
    loc: Loc,
}

#[derive(Clone, Copy)]
enum Opnd {
    Imm(i32),
    Reg(u8),
    Mem(M),
}

struct FnInfo {
    label: usize,
    params: Vec<Ty>,
    ret: Ty,
}

/// Error-reporting code that belongs to a function but is kept out of its body.
enum Stub {
    Fail { label: usize, message: usize, len: usize },
    Overflow { label: usize, a: Opnd, b: Opnd, record: usize },
    Bounds { label: usize, list: u8, index: u8, record: usize },
    /// `variable = variable.push(value)` when the item cannot be added in place.
    Push { label: usize, back: usize, variable: Loc, value: u8 },
}

struct Native<'a> {
    a: Asm,
    sources: &'a SourceMap,
    types: &'a TypeInfo,
    /// The program's functions by name; the runtime's have a `%` in front.
    fns: HashMap<String, FnInfo>,
    /// Positions of `call [rip + ...]` displacements and the import they call.
    iat_fixups: Vec<(usize, usize)>,
    /// Constant data, placed after the code.
    data: Vec<(usize, Vec<u8>)>,
    /// Positions of rip-relative displacements into the writable memory, and the offset there.
    bss_fixups: Vec<(usize, usize)>,
    /// Whether the program needs anything of `runtime.yui`.
    uses_heap: bool,
    /// The functions of `runtime.yui` that are called, and those not compiled yet.
    rt_seen: std::collections::HashSet<String>,
    rt_queue: Vec<String>,
    rt_copy: usize,
    /// Compiling `runtime.yui`: memory intrinsics are available and arithmetic is unchecked.
    in_runtime: bool,
    /// Count heap blocks and report at exit if any are left.
    check_heap: bool,
    rt_alloc: usize,
    rt_realloc: usize,
    rt_free: usize,
    rt_write: usize,
    rt_print_int: usize,
    rt_fail: usize,
    rt_overflow: usize,

    // ---- state of the function being compiled ----
    stubs: Vec<Stub>,
    scopes: Vec<HashMap<String, Local>>,
    /// Where each variable lives, by the start of its declaration, and where
    /// the hidden values of each loop live, by the start of the loop variable
    /// or method name.
    decl_loc: HashMap<(usize, u8), Loc>,
    free: Vec<u8>,
    /// (label to leave the loop, label to start the next round, number of
    /// scopes outside the loop body)
    loops: Vec<(usize, usize, usize)>,
    epilogue: usize,
    ret: Ty,
    /// Offset of the first spill slot, and how many are in use / were ever used.
    spill_base: i32,
    spill_depth: i32,
    spill_max: i32,
}

/// Compiles `program` (already type-checked) to a Windows x64 executable.
/// `sources` is used for the text of runtime errors.
pub fn build(program: &Program, sources: &SourceMap) -> Result<Vec<u8>, Unsupported> {
    let nowhere = Span::new(0, 0);
    if let Some(import) = program.imports.first() {
        return unsupported("`import`", import.span);
    }
    let mut a = Asm::default();
    let mut fns = HashMap::new();
    let mut decls = Vec::new();
    let Ok(runtime) = crate::parser::parse_file(RUNTIME, 0, "", true) else {
        return unsupported("its own runtime (it does not parse)", nowhere);
    };
    let mut runtime_fns = Vec::new();
    for item in &runtime.items {
        if let Item::Fn(f) = item {
            let ret = if f.ret.is_some() { Ty::Int } else { Ty::Unit };
            fns.insert(format!("%{}", f.name), FnInfo { label: a.label(), params: vec![Ty::Int; f.params.len()], ret });
            runtime_fns.push(f.clone());
        }
    }
    // Copying memory is machine code, but is called like a function of the runtime.
    let rt_copy = a.label();
    fns.insert("%__copy".to_string(), FnInfo { label: rt_copy, params: vec![Ty::Int; 3], ret: Ty::Unit });
    for item in &program.items {
        match item {
            Item::Fn(f) => {
                if !f.type_params.is_empty() {
                    return unsupported("generic functions", f.name_span);
                }
                if !f.requires.is_empty() || !f.ensures.is_empty() {
                    return unsupported("`requires` / `ensures`", f.name_span);
                }
                if f.params.len() > 4 {
                    return unsupported("functions with more than four parameters", f.name_span);
                }
                let mut params = Vec::new();
                for p in &f.params {
                    params.push(simple_type(&p.ty)?);
                }
                let ret = match &f.ret {
                    Some(t) => simple_type(t)?,
                    None => Ty::Unit,
                };
                fns.insert(f.name.clone(), FnInfo { label: a.label(), params, ret });
                decls.push(f.clone());
            }
            // Tests are not part of the built program.
            Item::Test(_) => {}
            Item::Struct(s) => return unsupported("structs", s.name_span),
            Item::Enum(e) => return unsupported("enums", e.name_span),
        }
    }
    let Some(main) = fns.get("main") else { return unsupported("a program without `main`", nowhere) };
    let main_label = main.label;

    let (rt_write, rt_print_int, rt_fail, rt_overflow) = (a.label(), a.label(), a.label(), a.label());
    let (rt_alloc, rt_realloc, rt_free) = (a.label(), a.label(), a.label());
    let mut n = Native {
        a,
        sources,
        types: &program.types,
        fns,
        iat_fixups: Vec::new(),
        data: Vec::new(),
        bss_fixups: Vec::new(),
        uses_heap: false,
        rt_seen: Default::default(),
        rt_queue: Vec::new(),
        rt_copy,
        in_runtime: false,
        check_heap: std::env::var_os("YUI_NATIVE_CHECK_HEAP").is_some(),
        rt_alloc,
        rt_realloc,
        rt_free,
        rt_write,
        rt_print_int,
        rt_fail,
        rt_overflow,
        stubs: Vec::new(),
        scopes: Vec::new(),
        decl_loc: HashMap::new(),
        free: Vec::new(),
        loops: Vec::new(),
        epilogue: 0,
        ret: Ty::Unit,
        spill_base: 0,
        spill_depth: 0,
        spill_max: 0,
    };

    // Entry point: run `main`, then leave with exit code 0.
    n.a.alu_imm(5, RSP, 40);
    n.call_label(main_label);
    let leak = n.a.label();
    if n.check_heap {
        n.rip_bss(&[0x48, 0x8B, 0x0D], LIVE_BLOCKS); // mov rcx, [live blocks]
        n.a.alu_rr(0x85, RCX, RCX);
        n.a.jcc(CC_NZ, leak);
    }
    n.a.buf.extend_from_slice(&[0x31, 0xC9]); // xor ecx, ecx
    n.call_import(EXIT_PROCESS);
    if n.check_heap {
        n.a.bind(leak);
        n.a.mov_imm64(RDX, STDERR);
        n.call_label(n.rt_print_int);
        n.write(b" block(s) of memory were not freed\n", STDERR);
        n.a.buf.extend_from_slice(&[0xB9, 3, 0, 0, 0]); // mov ecx, 3
        n.call_import(EXIT_PROCESS);
    }
    n.runtime();
    for f in &decls {
        n.function(f, &f.name)?;
    }
    if n.uses_heap {
        // Only the routines that are reached, to keep the executable small.
        n.in_runtime = true;
        while let Some(name) = n.rt_queue.pop() {
            if let Some(f) = runtime_fns.iter().find(|f| f.name == name) {
                n.function(f, &format!("%{name}"))?;
            }
        }
        n.heap_runtime();
    }
    for (label, bytes) in std::mem::take(&mut n.data) {
        n.a.bind(label);
        n.a.buf.extend_from_slice(&bytes);
    }
    let iat_fixups = std::mem::take(&mut n.iat_fixups);
    let bss_fixups = std::mem::take(&mut n.bss_fixups);
    Ok(write_pe(n.a.finish(), &iat_fixups, &bss_fixups))
}

fn simple_type(t: &TypeExpr) -> R<Ty> {
    match &t.kind {
        TypeKind::Named(name, args) if args.is_empty() => match name.as_str() {
            "Int" => Ok(Ty::Int),
            "Bool" => Ok(Ty::Bool),
            "Unit" => Ok(Ty::Unit),
            "String" => Ok(Ty::Str),
            other => unsupported(format!("the type `{other}`"), t.span),
        },
        TypeKind::Named(name, args) if name == "List" && args.len() == 1 => match simple_type(&args[0])? {
            Ty::Int => Ok(Ty::List),
            _ => unsupported("a list of anything but Ints", t.span),
        },
        _ => unsupported("this type", t.span),
    }
}

type Decls = Vec<(usize, String, u8)>;

/// Registers a hidden value of the loop at `id`, used every round.
fn hidden(id: usize, kind: u8, depth: u32, weight: &mut HashMap<String, usize>, decls: &mut Decls) {
    let name = format!("%{kind}@{id}");
    *weight.entry(name.clone()).or_default() += 2 * 8usize.saturating_pow(depth.min(6));
    decls.push((id, name, kind));
}

/// How often each variable name is used, counting uses inside loops as more.
fn weigh(e: &Expr, depth: u32, weight: &mut HashMap<String, usize>, decls: &mut Decls) {
    match &e.kind {
        ExprKind::List(items) => {
            for item in items {
                weigh(item, depth, weight, decls);
            }
        }
        ExprKind::Index(list, index) => {
            weigh(list, depth, weight, decls);
            weigh(index, depth, weight, decls);
        }
        ExprKind::MethodCall { recv, name_span, args, .. } => {
            weigh(recv, depth, weight, decls);
            for arg in args {
                let ExprKind::Closure { params, body, .. } = &arg.kind else {
                    weigh(arg, depth, weight, decls);
                    continue;
                };
                // The closure becomes the body of a loop over the list.
                let id = name_span.start;
                hidden(id, LIST, depth + 1, weight, decls);
                hidden(id, POS, depth + 1, weight, decls);
                match params.as_slice() {
                    // `fold`: the first parameter is the result so far.
                    [acc, item] => {
                        decls.push((id, acc.name.clone(), OUT));
                        decls.push((item.span.start, item.name.clone(), VAR));
                    }
                    _ => {
                        hidden(id, OUT, depth + 1, weight, decls);
                        for p in params {
                            decls.push((p.span.start, p.name.clone(), VAR));
                        }
                    }
                }
                weigh(body, depth + 1, weight, decls);
            }
        }
        ExprKind::Ident(name) => {
            *weight.entry(name.clone()).or_default() += 8usize.saturating_pow(depth.min(6));
        }
        ExprKind::Unary(_, x) => weigh(x, depth, weight, decls),
        ExprKind::Binary(_, l, r) => {
            weigh(l, depth, weight, decls);
            weigh(r, depth, weight, decls);
        }
        ExprKind::Call { args, .. } => {
            for arg in args {
                weigh(arg, depth, weight, decls);
            }
        }
        ExprKind::Str(parts) => {
            for part in parts {
                if let StrPart::Expr(x) = part {
                    weigh(x, depth, weight, decls);
                }
            }
        }
        ExprKind::If { cond, then, els } => {
            weigh(cond, depth, weight, decls);
            weigh_block(then, depth, weight, decls);
            if let Some(els) = els {
                weigh(els, depth, weight, decls);
            }
        }
        ExprKind::Block(b) => weigh_block(b, depth, weight, decls),
        ExprKind::Return(Some(v)) => weigh(v, depth, weight, decls),
        _ => {}
    }
}

fn weigh_block(b: &Block, depth: u32, weight: &mut HashMap<String, usize>, decls: &mut Decls) {
    for stmt in &b.stmts {
        match stmt {
            Stmt::Let { name, name_span, value, .. } => {
                weigh(value, depth, weight, decls);
                decls.push((name_span.start, name.clone(), VAR));
            }
            Stmt::Assign { target, value } => {
                weigh(target, depth, weight, decls);
                weigh(value, depth, weight, decls);
            }
            Stmt::While { cond, body, .. } => {
                weigh(cond, depth + 1, weight, decls);
                weigh_block(body, depth + 1, weight, decls);
            }
            Stmt::For { var, var_span, iter, end, body, .. } => {
                weigh(iter, depth, weight, decls);
                if let Some(end) = end {
                    weigh(end, depth, weight, decls);
                }
                decls.push((var_span.start, var.clone(), VAR));
                if end.is_some() {
                    decls.push((var_span.start, var.clone(), LIMIT));
                    // The loop itself reads the counter and the end every round.
                    *weight.entry(var.clone()).or_default() += 2 * 8usize.saturating_pow((depth + 1).min(6));
                } else {
                    hidden(var_span.start, LIST, depth + 1, weight, decls);
                    hidden(var_span.start, POS, depth + 1, weight, decls);
                }
                weigh_block(body, depth + 1, weight, decls);
            }
            Stmt::Assert { cond, .. } => weigh(cond, depth, weight, decls),
            Stmt::Expr(e) => weigh(e, depth, weight, decls),
        }
    }
}

impl Native<'_> {
    // ---- small pieces of code ----

    /// Whether evaluating `e` may call a function, which overwrites r8-r11.
    /// Anything with statements inside counts, to stay on the safe side.
    fn has_call(&self, e: &Expr) -> bool {
        match &e.kind {
            ExprKind::Int(_) | ExprKind::Bool(_) | ExprKind::Unit | ExprKind::Ident(_) => false,
            ExprKind::Unary(_, x) => self.has_call(x),
            ExprKind::Binary(_, l, r) => self.has_call(l) || self.has_call(r),
            // Reading from a list held by a variable is done in place.
            // (The same on a string is a call into the runtime.)
            ExprKind::Index(list, index) => self.list_var(list).is_none() || self.has_call(index),
            ExprKind::MethodCall { recv, name, args, .. } => {
                !(name == "len" && args.is_empty() && self.list_var(recv).is_some())
            }
            // Reading memory in the runtime is one instruction, not a call.
            ExprKind::Call { name, args, .. } if self.in_runtime && (name == "__load" || name == "__load8") => {
                args.iter().any(|arg| self.has_call(arg))
            }
            _ => true,
        }
    }

    fn call_label(&mut self, label: usize) {
        self.a.b(0xE8);
        self.a.rel32(label);
    }

    /// `call [rip + import]`
    fn call_import(&mut self, import: usize) {
        self.a.buf.extend_from_slice(&[0xFF, 0x15]);
        self.iat_fixups.push((self.a.buf.len(), import));
        self.a.d32(0);
    }

    /// `lea reg, [rip + label]`
    fn lea_label(&mut self, reg: u8, label: usize) {
        self.a.b(0x48 | (reg >> 3) << 2);
        self.a.b(0x8D);
        self.a.b((reg & 7) << 3 | 5);
        self.a.rel32(label);
    }

    /// Adds constant bytes to the executable and returns their label.
    fn constant(&mut self, bytes: &[u8]) -> usize {
        let label = self.a.label();
        self.data.push((label, bytes.to_vec()));
        label
    }

    fn mov(&mut self, dst: u8, src: u8) {
        if dst != src {
            self.a.mov_rr(dst, src);
        }
    }

    fn load_loc(&mut self, dst: u8, loc: Loc) {
        match loc {
            Loc::Reg(r) => self.mov(dst, r),
            Loc::Stack(off) => self.a.load(dst, M(RSP, off)),
        }
    }

    fn store_loc(&mut self, loc: Loc, src: u8) {
        match loc {
            Loc::Reg(r) => self.mov(r, src),
            Loc::Stack(off) => self.a.store(M(RSP, off), src),
        }
    }

    fn load_opnd(&mut self, dst: u8, o: Opnd) {
        match o {
            Opnd::Imm(n) => self.a.mov_imm64(dst, n as i64),
            Opnd::Reg(r) => self.mov(dst, r),
            Opnd::Mem(m) => self.a.load(dst, m),
        }
    }

    fn take_temp(&mut self, span: Span) -> R<u8> {
        match self.free.pop() {
            Some(t) => Ok(t),
            None => unsupported("an expression nested this deeply", span),
        }
    }

    /// Emits `opcode` followed by the rip-relative address of writable memory at `offset`.
    fn rip_bss(&mut self, opcode: &[u8], offset: usize) {
        self.a.buf.extend_from_slice(opcode);
        self.bss_fixups.push((self.a.buf.len(), offset));
        self.a.d32(0);
    }

    /// Calls a function of `runtime.yui`.
    fn call_rt(&mut self, name: &str) {
        self.need_rt(name);
        let label = self.fns[&format!("%{name}")].label;
        self.call_label(label);
    }

    /// Notes that the function `name` of `runtime.yui` has to be in the executable.
    fn need_rt(&mut self, name: &str) {
        self.uses_heap = true;
        if self.rt_seen.insert(name.to_string()) {
            self.rt_queue.push(name.to_string());
        }
    }

    /// One more owner for the list or string in `reg`.
    fn retain(&mut self, reg: u8, ty: Ty) {
        match ty {
            Ty::List => self.a.inc_mem(M(reg, 0)),
            Ty::Str => {
                // A constant has a negative count and is left alone.
                let constant = self.a.label();
                self.a.cmp_imm(M(reg, 0), 0);
                self.a.jcc(CC_L, constant);
                self.a.inc_mem(M(reg, 0));
                self.a.bind(constant);
            }
            _ => {}
        }
    }

    /// A string that is part of the executable.
    fn str_constant(&mut self, text: &str) -> usize {
        let mut block = Vec::new();
        block.extend((-1i64).to_le_bytes());
        block.extend((text.len() as u64).to_le_bytes());
        block.extend((text.len() as u64).to_le_bytes());
        block.extend(text.as_bytes());
        self.constant(&block)
    }

    /// The register holding the value at `loc`, loading it into `scratch` if it is in the frame.
    fn reg(&mut self, loc: Loc, scratch: u8) -> u8 {
        match loc {
            Loc::Reg(r) => r,
            Loc::Stack(off) => {
                self.a.load(scratch, M(RSP, off));
                scratch
            }
        }
    }

    /// `mov reg, [list + index * 8 + ELEMENTS]` (0x8B) or the store the other way (0x89).
    fn element(&mut self, opcode: u8, reg: u8, list: u8, index: u8) {
        self.a.b(0x48 | (reg >> 3) << 2 | (index >> 3) << 1 | (list >> 3));
        self.a.b(opcode);
        self.a.b(0x44 | (reg & 7) << 3);
        self.a.b(0xC0 | (index & 7) << 3 | (list & 7));
        self.a.b(ELEMENTS as u8);
    }

    /// Makes every temp but `except` available and returns the previous set,
    /// to be put back afterwards. Only for an expression with a call in it:
    /// whoever held a value in a temp has parked it by then, so the temps are
    /// in fact unused, whatever the nesting.
    fn fresh_temps(&mut self, except: u8) -> Vec<u8> {
        std::mem::replace(&mut self.free, TEMPS.iter().copied().filter(|t| *t != except).collect())
    }

    fn push_spill(&mut self) -> M {
        let slot = M(RSP, self.spill_base + 8 * self.spill_depth);
        self.spill_depth += 1;
        self.spill_max = self.spill_max.max(self.spill_depth);
        slot
    }

    // ---- the runtime: output and errors ----

    fn runtime(&mut self) {
        // write(rcx = text, rdx = length, r8 = STDOUT or STDERR)
        self.a.bind(self.rt_write);
        self.a.push(RSI);
        self.a.push(RDI);
        self.a.alu_imm(5, RSP, 56);
        self.a.mov_rr(RSI, RCX);
        self.a.mov_rr(RDI, RDX);
        self.a.mov_rr(RCX, R8);
        self.call_import(GET_STD_HANDLE);
        self.a.mov_rr(RCX, RAX);
        self.a.mov_rr(RDX, RSI);
        self.a.mov_rr(R8, RDI);
        self.a.lea(R9, M(RSP, 40));
        self.a.store_imm(M(RSP, 32), 0);
        self.call_import(WRITE_FILE);
        self.a.alu_imm(0, RSP, 56);
        self.a.pop(RDI);
        self.a.pop(RSI);
        self.a.b(0xC3);

        // print_int(rcx = value, rdx = STDOUT or STDERR): the digits are
        // written backwards from the end of a buffer on the stack.
        let (digits, positive, unsigned) = (self.a.label(), self.a.label(), self.a.label());
        self.a.bind(self.rt_print_int);
        self.a.alu_imm(5, RSP, 72);
        self.a.mov_rr(R10, RDX);
        self.a.mov_rr(R11, RCX);
        self.a.mov_rr(RAX, RCX);
        self.a.lea(R9, M(RSP, 72));
        self.a.alu_rr(0x85, RAX, RAX); // test rax, rax
        self.a.jcc(CC_NS, positive);
        self.a.buf.extend_from_slice(&[0x48, 0xF7, 0xD8]); // neg rax
        self.a.bind(positive);
        self.a.buf.extend_from_slice(&[0xB9, 10, 0, 0, 0]); // mov ecx, 10
        self.a.bind(digits);
        self.a.buf.extend_from_slice(&[0x31, 0xD2]); // xor edx, edx
        self.a.buf.extend_from_slice(&[0x48, 0xF7, 0xF1]); // div rcx
        self.a.buf.extend_from_slice(&[0x80, 0xC2, b'0']); // add dl, '0'
        self.a.alu_imm(5, R9, 1);
        self.a.buf.extend_from_slice(&[0x41, 0x88, 0x11]); // mov [r9], dl
        self.a.alu_rr(0x85, RAX, RAX);
        self.a.jcc(CC_NZ, digits);
        self.a.alu_rr(0x85, R11, R11);
        self.a.jcc(CC_NS, unsigned);
        self.a.alu_imm(5, R9, 1);
        self.a.buf.extend_from_slice(&[0x41, 0xC6, 0x01, b'-']); // mov byte [r9], '-'
        self.a.bind(unsigned);
        self.a.mov_rr(RCX, R9);
        self.a.lea(RDX, M(RSP, 72));
        self.a.alu_rr(0x29, RDX, R9); // sub rdx, r9
        self.a.mov_rr(R8, R10);
        self.call_label(self.rt_write);
        self.a.alu_imm(0, RSP, 72);
        self.a.b(0xC3);

        // fail(rcx = message, rdx = length): report an error and exit with 1.
        self.a.bind(self.rt_fail);
        self.a.alu_imm(5, RSP, 40);
        self.a.mov_imm64(R8, STDERR);
        self.call_label(self.rt_write);
        self.a.buf.extend_from_slice(&[0xB9, 1, 0, 0, 0]); // mov ecx, 1
        self.call_import(EXIT_PROCESS);

        // overflow(rcx = left operand, rdx = right operand, r8 = record):
        // the record holds three lengths (4 bytes each) and then three pieces
        // of text; the operands are printed between the pieces.
        self.a.bind(self.rt_overflow);
        self.a.alu_imm(5, RSP, 40);
        self.a.mov_rr(RBX, RCX);
        self.a.mov_rr(RSI, RDX);
        self.a.mov_rr(RDI, R8);
        self.a.buf.extend_from_slice(&[0x48, 0x8D, 0x4F, 12]); // lea rcx, [rdi + 12]
        self.a.buf.extend_from_slice(&[0x8B, 0x17]); // mov edx, [rdi]
        self.a.mov_imm64(R8, STDERR);
        self.call_label(self.rt_write);
        self.a.mov_rr(RCX, RBX);
        self.a.mov_imm64(RDX, STDERR);
        self.call_label(self.rt_print_int);
        self.a.buf.extend_from_slice(&[0x8B, 0x07]); // mov eax, [rdi]
        self.a.buf.extend_from_slice(&[0x48, 0x8D, 0x4C, 0x07, 12]); // lea rcx, [rdi + rax + 12]
        self.a.buf.extend_from_slice(&[0x8B, 0x57, 4]); // mov edx, [rdi + 4]
        self.a.mov_imm64(R8, STDERR);
        self.call_label(self.rt_write);
        self.a.mov_rr(RCX, RSI);
        self.a.mov_imm64(RDX, STDERR);
        self.call_label(self.rt_print_int);
        self.a.buf.extend_from_slice(&[0x8B, 0x07]); // mov eax, [rdi]
        self.a.buf.extend_from_slice(&[0x03, 0x47, 4]); // add eax, [rdi + 4]
        self.a.buf.extend_from_slice(&[0x48, 0x8D, 0x4C, 0x07, 12]); // lea rcx, [rdi + rax + 12]
        self.a.buf.extend_from_slice(&[0x8B, 0x57, 8]); // mov edx, [rdi + 8]
        self.a.mov_imm64(R8, STDERR);
        self.call_label(self.rt_write);
        self.a.buf.extend_from_slice(&[0xB9, 1, 0, 0, 0]); // mov ecx, 1
        self.call_import(EXIT_PROCESS);
    }

    /// `rcx = the process heap`. Windows keeps it in the process environment
    /// block; this is what `GetProcessHeap` does.
    fn process_heap(&mut self) {
        self.a.buf.extend_from_slice(&[0x65, 0x48, 0x8B, 0x0C, 0x25, 0x60, 0, 0, 0]); // mov rcx, gs:[0x60]
        self.a.buf.extend_from_slice(&[0x48, 0x8B, 0x49, 0x30]); // mov rcx, [rcx + 0x30]
    }

    /// Memory from the operating system, for the intrinsics of `runtime.yui`.
    fn heap_runtime(&mut self) {
        let oom = self.a.label();

        // alloc(rcx = bytes) -> rax
        self.a.bind(self.rt_alloc);
        self.a.alu_imm(5, RSP, 40);
        self.a.mov_rr(R8, RCX);
        self.process_heap();
        self.a.buf.extend_from_slice(&[0x31, 0xD2]); // xor edx, edx
        self.call_import(HEAP_ALLOC);
        self.a.alu_rr(0x85, RAX, RAX);
        self.a.jcc(CC_Z, oom);
        if self.check_heap {
            self.rip_bss(&[0x48, 0xFF, 0x05], LIVE_BLOCKS); // inc qword [live blocks]
        }
        self.a.alu_imm(0, RSP, 40);
        self.a.b(0xC3);

        // realloc(rcx = block, rdx = bytes) -> rax
        self.a.bind(self.rt_realloc);
        self.a.alu_imm(5, RSP, 40);
        self.a.mov_rr(R8, RCX);
        self.a.mov_rr(R9, RDX);
        self.process_heap();
        self.a.buf.extend_from_slice(&[0x31, 0xD2]); // xor edx, edx
        self.call_import(HEAP_REALLOC);
        self.a.alu_rr(0x85, RAX, RAX);
        self.a.jcc(CC_Z, oom);
        self.a.alu_imm(0, RSP, 40);
        self.a.b(0xC3);

        // free(rcx = block)
        self.a.bind(self.rt_free);
        self.a.alu_imm(5, RSP, 40);
        self.a.mov_rr(R8, RCX);
        self.process_heap();
        self.a.buf.extend_from_slice(&[0x31, 0xD2]); // xor edx, edx
        self.call_import(HEAP_FREE);
        if self.check_heap {
            self.rip_bss(&[0x48, 0xFF, 0x0D], LIVE_BLOCKS); // dec qword [live blocks]
        }
        self.a.alu_imm(0, RSP, 40);
        self.a.b(0xC3);

        // copy(rcx = to, rdx = from, r8 = bytes): a few bytes one at a time,
        // more with the processor's block copy, which is slow to start.
        let (block, bytes, done) = (self.a.label(), self.a.label(), self.a.label());
        self.a.bind(self.rt_copy);
        self.a.alu_imm(7, R8, 32);
        self.a.jcc(CC_AE, block);
        self.a.alu_rr(0x85, R8, R8);
        self.a.jcc(CC_Z, done);
        self.a.bind(bytes);
        self.a.buf.extend_from_slice(&[0x8A, 0x02]); // mov al, [rdx]
        self.a.buf.extend_from_slice(&[0x88, 0x01]); // mov [rcx], al
        self.a.alu_imm(0, RCX, 1);
        self.a.alu_imm(0, RDX, 1);
        self.a.alu_imm(5, R8, 1);
        self.a.jcc(CC_NZ, bytes);
        self.a.bind(done);
        self.a.b(0xC3);
        self.a.bind(block);
        self.a.push(RSI);
        self.a.push(RDI);
        self.a.mov_rr(RDI, RCX);
        self.a.mov_rr(RSI, RDX);
        self.a.mov_rr(RCX, R8);
        self.a.buf.extend_from_slice(&[0xF3, 0xA4]); // rep movsb
        self.a.pop(RDI);
        self.a.pop(RSI);
        self.a.b(0xC3);

        // Reached with the stack as inside the routines above.
        self.a.bind(oom);
        let text = b"error: out of memory\n";
        let message = self.constant(text);
        self.lea_label(RCX, message);
        self.a.mov_imm64(RDX, text.len() as i64);
        self.call_label(self.rt_fail);
    }

    /// The text `yui run` prints for a runtime error.
    fn error_text(&self, d: &Diagnostic) -> String {
        format!("{}\n1 error(s)\n", render_human(d, self.sources))
    }

    /// Reserves code that reports a fixed error and returns its label. The
    /// code is emitted after the function, out of the way of the normal path.
    fn fail_stub(&mut self, d: Diagnostic) -> usize {
        let text = self.error_text(&d);
        let message = self.constant(text.as_bytes());
        let label = self.a.label();
        self.stubs.push(Stub::Fail { label, message, len: text.len() });
        label
    }

    /// Emits the error-reporting code reserved while compiling a function.
    fn emit_stubs(&mut self) {
        for stub in std::mem::take(&mut self.stubs) {
            match stub {
                Stub::Fail { label, message, len } => {
                    self.a.bind(label);
                    self.lea_label(RCX, message);
                    self.a.mov_imm64(RDX, len as i64);
                    self.call_label(self.rt_fail);
                }
                Stub::Overflow { label, a, b, record } => {
                    self.a.bind(label);
                    // Neither operand is in rcx or rdx, so the order does not matter.
                    self.load_opnd(RCX, a);
                    self.load_opnd(RDX, b);
                    self.lea_label(R8, record);
                    self.call_label(self.rt_overflow);
                }
                Stub::Push { label, back, variable, value } => {
                    self.a.bind(label);
                    self.a.mov_rr(RDX, value);
                    self.load_loc(RCX, variable);
                    self.call_rt("list_push");
                    self.store_loc(variable, RAX);
                    self.a.jmp(back);
                }
                Stub::Bounds { label, list, index, record } => {
                    self.a.bind(label);
                    // The index is in neither rcx nor rdx; the list may be in rcx.
                    self.a.load(RDX, M(list, LENGTH));
                    self.mov(RCX, index);
                    self.lea_label(R8, record);
                    self.call_label(self.rt_overflow);
                }
            }
        }
    }

    /// Reserves code that reports an overflow of `a op b` and returns its label.
    fn overflow_stub(&mut self, op: BinOp, a: Opnd, b: Opnd, span: Span) -> usize {
        let d = Diagnostic::new("E0503", format!("integer overflow in `\u{1} {} \u{2}`", op.symbol()), span);
        let record = self.record(&d);
        let label = self.a.label();
        self.stubs.push(Stub::Overflow { label, a, b, record });
        label
    }

    /// Reserves code that reports `index` (a register) as out of bounds for
    /// the list whose address is in the register `list`.
    fn bounds_stub(&mut self, list: u8, index: u8, span: Span) -> usize {
        let d = Diagnostic::new("E0505", "index \u{1} is out of bounds for a list of length \u{2}", span)
            .hint("use `.get(i)` to receive an Option instead of failing");
        let record = self.record(&d);
        let label = self.a.label();
        self.stubs.push(Stub::Bounds { label, list, index, record });
        label
    }

    /// The text of an error whose message has two numbers in it, marked
    /// `\u{1}` and `\u{2}`, cut at the marks for `rt_overflow`.
    fn record(&mut self, d: &Diagnostic) -> usize {
        let text = self.error_text(d);
        let (prefix, rest) = text.split_once('\u{1}').unwrap_or((&text, ""));
        let (middle, suffix) = rest.split_once('\u{2}').unwrap_or((rest, ""));
        let mut record = Vec::new();
        for piece in [prefix, middle, suffix] {
            record.extend((piece.len() as u32).to_le_bytes());
        }
        for piece in [prefix, middle, suffix] {
            record.extend(piece.as_bytes());
        }
        self.constant(&record)
    }

    // ---- functions ----

    /// Compiles `f`, which is registered in `fns` as `key`.
    fn function(&mut self, f: &FnDecl, key: &str) -> R<()> {
        // Decide where every variable lives: the most used ones in registers.
        let mut weight = HashMap::new();
        let mut decls: Decls = f.params.iter().map(|p| (p.span.start, p.name.clone(), VAR)).collect();
        weigh_block(&f.body, 0, &mut weight, &mut decls);
        let mut order: Vec<usize> = (0..decls.len()).collect();
        order.sort_by_key(|i| std::cmp::Reverse(weight.get(&decls[*i].1).copied().unwrap_or(0)));
        self.decl_loc.clear();
        let mut used = Vec::new();
        let mut stack_slots = 0;
        for (rank, i) in order.into_iter().enumerate() {
            let loc = match SAVED.get(rank) {
                Some(reg) => {
                    used.push(*reg);
                    Loc::Reg(*reg)
                }
                None => {
                    stack_slots += 1;
                    // Above the 32 bytes every call may use for its arguments.
                    Loc::Stack(32 + 8 * (stack_slots - 1))
                }
            };
            let (id, _, kind) = &decls[i];
            self.decl_loc.insert((*id, *kind), loc);
        }
        self.spill_base = 32 + 8 * stack_slots;
        self.spill_depth = 0;
        self.spill_max = 0;
        self.free = TEMPS.to_vec();
        self.loops.clear();
        self.epilogue = self.a.label();
        let info = &self.fns[key];
        let (label, ret) = (info.label, info.ret);
        self.ret = ret;

        self.a.bind(label);
        for reg in &used {
            self.a.push(*reg);
        }
        self.a.alu_imm(5, RSP, 0);
        let frame_at = [self.a.buf.len() - 4, 0];
        let mut scope = HashMap::new();
        for (i, p) in f.params.iter().enumerate() {
            let loc = self.decl_loc[&(p.span.start, VAR)];
            self.store_loc(loc, ARG_REGS[i]);
            scope.insert(p.name.clone(), Local { ty: simple_type(&p.ty)?, loc });
        }
        self.scopes = vec![scope];

        let t = self.take_temp(f.name_span)?;
        let ty = self.block(&f.body, t)?;
        if ret != Ty::Unit && ty != Ty::Never {
            self.a.mov_rr(RAX, t);
        }
        self.free.push(t);

        self.a.bind(self.epilogue);
        // The parameters are the function's own references.
        self.release_scopes(0, (ret != Ty::Unit).then_some(RAX));
        self.a.alu_imm(0, RSP, 0);
        let frame_at = [frame_at[0], self.a.buf.len() - 4];
        for reg in used.iter().rev() {
            self.a.pop(*reg);
        }
        self.a.b(0xC3);
        self.emit_stubs();

        // The stack must be 16-byte aligned at every call. On entry it is 8
        // off, and each saved register moves it by another 8.
        let mut frame = (self.spill_base + 8 * self.spill_max + 15) / 16 * 16;
        if used.len() % 2 == 0 {
            frame += 8;
        }
        for at in frame_at {
            self.a.buf[at..at + 4].copy_from_slice(&frame.to_le_bytes());
        }
        Ok(())
    }

    fn lookup(&self, name: &str, span: Span) -> R<Local> {
        match self.scopes.iter().rev().find_map(|s| s.get(name)) {
            Some(local) => Ok(*local),
            None => unsupported(format!("the name `{name}`"), span),
        }
    }

    /// Adds a variable to the innermost scope.
    fn declare(&mut self, name: &str, local: Local) {
        let scope = self.scopes.last_mut().expect("scope");
        if let Some(old) = scope.insert(name.to_string(), local) {
            if old.ty.heap() {
                // Shadowed, but it still holds a list until the scope ends.
                let hidden = format!("{name}%{}", scope.len());
                scope.insert(hidden, old);
            }
        }
    }

    /// Gives up the lists and strings held by the variables of the scopes
    /// from `from` on. `keep` is a register whose value must survive.
    fn release_scopes(&mut self, from: usize, keep: Option<u8>) {
        let mut locs: Vec<Local> =
            self.scopes[from..].iter().flat_map(|s| s.values()).filter(|l| l.ty.heap()).copied().collect();
        if locs.is_empty() {
            return;
        }
        // In a fixed order, so that the same program always gives the same executable.
        locs.sort_by_key(|local| match local.loc {
            Loc::Reg(r) => r as i32,
            Loc::Stack(off) => 100 + off,
        });
        let slot = keep.map(|reg| {
            let slot = self.push_spill();
            self.a.store(slot, reg);
            slot
        });
        for local in locs {
            self.load_loc(RCX, local.loc);
            self.call_rt(local.ty.release());
        }
        if let (Some(reg), Some(slot)) = (keep, slot) {
            self.a.load(reg, slot);
            self.spill_depth -= 1;
        }
    }

    /// Gives up the list in rcx without losing the value in `keep`.
    fn release_keeping(&mut self, keep: u8) {
        let slot = self.push_spill();
        self.a.store(slot, keep);
        self.call_rt("list_release");
        self.a.load(keep, slot);
        self.spill_depth -= 1;
    }

    /// A value that is dropped must not be counted: nothing would release it.
    /// (The type checker already rejects unused values.)
    fn discard(&self, ty: Ty, span: Span) -> R<()> {
        if ty.heap() {
            return unsupported("a list or a string that is not used", span);
        }
        Ok(())
    }

    /// The variable `e` names, if it holds a list or a string.
    fn heap_var(&self, e: &Expr) -> Option<Local> {
        let ExprKind::Ident(name) = &e.kind else { return None };
        let local = self.scopes.iter().rev().find_map(|s| s.get(name.as_str()))?;
        local.ty.heap().then_some(*local)
    }

    /// Puts the value of `e` in a new spill slot, using `dst` on the way.
    /// A list or string held by a variable is only borrowed (the variable
    /// keeps it alive); otherwise the slot owns a reference. Returns the
    /// slot, whether it owns, and the type.
    fn park(&mut self, e: &Expr, dst: u8) -> R<(M, bool, Ty)> {
        let (owned, ty) = match self.heap_var(e) {
            Some(local) => {
                self.load_loc(dst, local.loc);
                (false, local.ty)
            }
            None => (true, self.eval(e, dst)?),
        };
        let slot = self.push_spill();
        self.a.store(slot, dst);
        Ok((slot, owned, ty))
    }

    /// Calls `routine` of the runtime on one or two strings and leaves its
    /// result in rax. `left` is a parked string (see `park`); its slot and
    /// the one of `right` are freed, and owned references given up.
    fn str_op(&mut self, routine: &str, left: (M, bool), right: Option<&Expr>, dst: u8) -> R<()> {
        let mut second = None;
        if let Some(right) = right {
            let (slot, owned, ty) = self.park(right, dst)?;
            if ty != Ty::Str {
                return unsupported("this operation on a String and something else", right.span);
            }
            second = Some((slot, owned));
        }
        self.a.load(RCX, left.0);
        if let Some((slot, _)) = second {
            self.a.load(RDX, slot);
        }
        self.call_rt(routine);
        for (slot, owned) in [Some(left), second].into_iter().flatten() {
            if owned {
                // The slot now keeps the result while the string is released.
                self.a.load(RCX, slot);
                self.a.store(slot, RAX);
                self.call_rt("str_release");
                self.a.load(RAX, slot);
            }
        }
        self.spill_depth -= 1 + second.is_some() as i32;
        Ok(())
    }

    /// What `e` evaluates to, as far as it can be told without compiling it.
    fn type_of(&self, e: &Expr) -> Option<Ty> {
        match &e.kind {
            ExprKind::Int(_) | ExprKind::Index(..) => Some(Ty::Int),
            ExprKind::Bool(_) => Some(Ty::Bool),
            ExprKind::Str(_) => Some(Ty::Str),
            ExprKind::List(_) => Some(Ty::List),
            ExprKind::Ident(name) => self.scopes.iter().rev().find_map(|s| s.get(name.as_str())).map(|l| l.ty),
            ExprKind::Call { name, local: false, .. } => self.fns.get(name).map(|f| f.ret),
            ExprKind::Unary(UnOp::Neg, _) => Some(Ty::Int),
            ExprKind::Unary(UnOp::Not, _) => Some(Ty::Bool),
            ExprKind::Binary(op, ..) => match op {
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => Some(Ty::Int),
                _ => Some(Ty::Bool),
            },
            ExprKind::MethodCall { name, .. } => match name.as_str() {
                "to_string" => Some(Ty::Str),
                "push" | "map" | "filter" => Some(Ty::List),
                "len" => Some(Ty::Int),
                "any" | "all" | "is_empty" | "contains" | "starts_with" | "ends_with" => Some(Ty::Bool),
                _ => None,
            },
            ExprKind::Block(b) => self.type_of_block(b),
            ExprKind::If { then, els, .. } => {
                self.type_of_block(then).or_else(|| els.as_ref().and_then(|e| self.type_of(e)))
            }
            _ => None,
        }
    }

    fn type_of_block(&self, b: &Block) -> Option<Ty> {
        match b.stmts.last() {
            Some(Stmt::Expr(e)) => self.type_of(e),
            _ => None,
        }
    }

    /// The variable `e` names, if it holds a list.
    fn list_var(&self, e: &Expr) -> Option<Local> {
        let ExprKind::Ident(name) = &e.kind else { return None };
        let local = self.scopes.iter().rev().find_map(|s| s.get(name.as_str()))?;
        (local.ty == Ty::List).then_some(*local)
    }

    /// Evaluates `e`, which must be a list, into `dst`; `dst` then owns a reference.
    fn owned_list(&mut self, e: &Expr, dst: u8) -> R<()> {
        match self.eval(e, dst)? {
            Ty::List => Ok(()),
            _ => unsupported("this operation on anything but a list of Ints", e.span),
        }
    }

    fn expect_int(&self, ty: Ty, span: Span) -> R<()> {
        match ty {
            Ty::Int => Ok(()),
            _ => unsupported("a list of anything but Ints", span),
        }
    }

    // ---- statements ----

    /// Compiles a block; its value, if any, goes to `dst`.
    fn block(&mut self, b: &Block, dst: u8) -> R<Ty> {
        self.scopes.push(HashMap::new());
        let outer = self.fresh_temps(dst);
        let mut ty = Ty::Unit;
        let last = b.stmts.len().wrapping_sub(1);
        for (i, stmt) in b.stmts.iter().enumerate() {
            ty = match stmt {
                Stmt::Expr(e) if i == last => self.eval(e, dst)?,
                Stmt::Expr(e) => {
                    let ty = self.eval(e, dst)?;
                    self.discard(ty, e.span)?;
                    Ty::Unit
                }
                other => {
                    self.stmt(other)?;
                    Ty::Unit
                }
            };
        }
        if ty != Ty::Never {
            let keep = (ty != Ty::Unit && TEMPS.contains(&dst)).then_some(dst);
            self.release_scopes(self.scopes.len() - 1, keep);
        }
        self.scopes.pop();
        self.free = outer;
        Ok(ty)
    }

    /// Evaluates `value` into the variable at `loc`.
    fn eval_into(&mut self, value: &Expr, loc: Loc, direct: bool) -> R<Ty> {
        if let (Loc::Reg(r), true) = (loc, direct) {
            return self.eval(value, r);
        }
        if self.has_call(value) {
            let outer = self.fresh_temps(TEMPS[0]);
            let ty = self.eval(value, TEMPS[0])?;
            self.store_loc(loc, TEMPS[0]);
            self.free = outer;
            return Ok(ty);
        }
        let t = self.take_temp(value.span)?;
        let ty = self.eval(value, t)?;
        self.store_loc(loc, t);
        self.free.push(t);
        Ok(ty)
    }

    fn stmt(&mut self, stmt: &Stmt) -> R<()> {
        match stmt {
            Stmt::Let { name, name_span, value, .. } => {
                let loc = self.decl_loc[&(name_span.start, VAR)];
                // The new variable is not visible in its own initializer.
                let ty = self.eval_into(value, loc, true)?;
                let known = match ty {
                    Ty::Int | Ty::Bool | Ty::Str => true,
                    // An empty list says nothing about its items; ask the type checker.
                    // (`_` is never read, so its items do not matter.)
                    Ty::List => {
                        name == "_" || self.types.var_types.get(&name_span.start).is_some_and(|t| t == "List<Int>")
                    }
                    _ => false,
                };
                if !known {
                    return unsupported("a variable that is not an Int, a Bool, a String or a List<Int>", *name_span);
                }
                // `_` cannot be read, but what it holds is still released with the scope.
                if name != "_" || ty.heap() {
                    self.declare(name, Local { ty, loc });
                }
            }
            Stmt::Assign { target, value } => {
                let ExprKind::Ident(name) = &target.kind else {
                    return unsupported("assigning to anything but a variable", target.span);
                };
                let local = self.lookup(name, target.span)?;
                if local.ty.heap() {
                    return self.assign_heap(name, local, value);
                }
                // `x = x + 1` can be computed in place: x is read before it is written.
                let updates_itself = match &value.kind {
                    ExprKind::Binary(_, l, r) => {
                        matches!(&l.kind, ExprKind::Ident(n) if n == name) && !mentions(r, name)
                    }
                    _ => false,
                };
                let direct = !self.has_call(value) && (updates_itself || !mentions(value, name));
                self.eval_into(value, local.loc, direct)?;
            }
            Stmt::While { cond, body, .. } => {
                let (top, exit) = (self.a.label(), self.a.label());
                self.a.bind(top);
                self.cond(cond, exit)?;
                self.loops.push((exit, top, self.scopes.len()));
                let t = self.take_temp(cond.span)?;
                let ty = self.block(body, t)?;
                self.discard(ty, body.span)?;
                self.free.push(t);
                self.loops.pop();
                self.a.jmp(top);
                self.a.bind(exit);
            }
            Stmt::For { var, var_span, iter, end: Some(end), body, .. } => {
                let counter = self.decl_loc[&(var_span.start, VAR)];
                let limit = self.decl_loc[&(var_span.start, LIMIT)];
                self.eval_into(iter, counter, false)?;
                self.eval_into(end, limit, false)?;
                let (top, next, exit) = (self.a.label(), self.a.label(), self.a.label());
                self.a.bind(top);
                self.load_loc(RAX, counter);
                match limit {
                    Loc::Reg(r) => self.a.alu_rr(0x39, RAX, r),
                    Loc::Stack(off) => self.a.alu_m(0x3B, RAX, M(RSP, off)),
                }
                self.a.jcc(CC_GE, exit);
                self.scopes.push(HashMap::from([(var.clone(), Local { ty: Ty::Int, loc: counter })]));
                self.loops.push((exit, next, self.scopes.len()));
                let t = self.take_temp(*var_span)?;
                let ty = self.block(body, t)?;
                self.discard(ty, body.span)?;
                self.free.push(t);
                self.loops.pop();
                self.scopes.pop();
                self.a.bind(next);
                // The counter is below the end here, so adding one cannot overflow.
                match counter {
                    Loc::Reg(r) => self.a.alu_imm(0, r, 1),
                    Loc::Stack(off) => self.a.inc_mem(M(RSP, off)),
                }
                self.a.jmp(top);
                self.a.bind(exit);
            }
            Stmt::For { var, var_span, iter, end: None, body, .. } => {
                let list = self.decl_loc[&(var_span.start, LIST)];
                let pos = self.decl_loc[&(var_span.start, POS)];
                let item = self.decl_loc[&(var_span.start, VAR)];
                // The loop holds its own reference: the list it walks stays as
                // it is even if the body changes the variable it came from.
                if self.eval_into(iter, list, true)? != Ty::List {
                    return unsupported("`for` over anything but a list of Ints", iter.span);
                }
                self.set_loc(pos, 0);
                let (top, next, exit) = (self.a.label(), self.a.label(), self.a.label());
                self.scopes.push(HashMap::from([
                    ("%for".to_string(), Local { ty: Ty::List, loc: list }),
                    (var.clone(), Local { ty: Ty::Int, loc: item }),
                ]));
                self.a.bind(top);
                self.next_item(list, pos, item, exit);
                self.loops.push((exit, next, self.scopes.len()));
                let t = self.take_temp(*var_span)?;
                let ty = self.block(body, t)?;
                self.discard(ty, body.span)?;
                self.free.push(t);
                self.loops.pop();
                self.a.bind(next);
                self.step(pos);
                self.a.jmp(top);
                self.a.bind(exit);
                self.scopes.pop();
                self.load_loc(RCX, list);
                self.call_rt("list_release");
            }
            Stmt::Assert { span, .. } => return unsupported("`assert`", *span),
            Stmt::Expr(e) => {
                let t = self.take_temp(e.span)?;
                let ty = self.eval(e, t)?;
                self.discard(ty, e.span)?;
                self.free.push(t);
            }
        }
        Ok(())
    }

    /// `name = value` for a list or string variable: what it held is given up.
    fn assign_heap(&mut self, name: &str, local: Local, value: &Expr) -> R<()> {
        // `xs = xs.push(v)` hands the variable's own reference to `push`, which
        // can then add the item in place instead of copying the list.
        if let ExprKind::MethodCall { recv, name: method, args, .. } = &value.kind {
            if local.ty == Ty::List && method == "push" && args.len() == 1 && matches!(&recv.kind, ExprKind::Ident(n) if n == name) {
                let t = self.take_temp(value.span)?;
                let ty = self.eval(&args[0], t)?;
                self.expect_int(ty, args[0].span)?;
                // The usual case is written out here: the variable is the only
                // owner and there is room. Everything else goes to `list_push`.
                let (slow, back) = (self.a.label(), self.a.label());
                self.stubs.push(Stub::Push { label: slow, back, variable: local.loc, value: t });
                self.uses_heap = true;
                let l = self.reg(local.loc, RCX);
                self.a.cmp_imm(M(l, 0), 1);
                self.a.jcc(CC_NE, slow);
                self.a.load(RAX, M(l, LENGTH));
                self.a.alu_m(0x3B, RAX, M(l, CAPACITY));
                self.a.jcc(CC_GE, slow);
                self.element(0x89, t, l, RAX);
                self.a.inc_mem(M(l, LENGTH));
                self.a.bind(back);
                self.free.push(t);
                return Ok(());
            }
        }
        let t = self.take_temp(value.span)?;
        if self.eval(value, t)? != local.ty {
            return unsupported("this assignment", value.span);
        }
        self.load_loc(RCX, local.loc);
        self.store_loc(local.loc, t);
        self.free.push(t);
        self.call_rt(local.ty.release());
        Ok(())
    }

    /// Start of a round of a loop over a list: leaves through `exit` at the
    /// end of the list, otherwise puts the item at `pos` into `item`.
    fn next_item(&mut self, list: Loc, pos: Loc, item: Loc, exit: usize) {
        let l = self.reg(list, RCX);
        let p = self.reg(pos, RAX);
        self.a.alu_m(0x3B, p, M(l, LENGTH)); // cmp p, [l + LENGTH]
        self.a.jcc(CC_GE, exit);
        match item {
            Loc::Reg(r) => self.element(0x8B, r, l, p),
            Loc::Stack(off) => {
                self.element(0x8B, RDX, l, p);
                self.a.store(M(RSP, off), RDX);
            }
        }
    }

    /// Moves a position on by one. It is below a length, so this cannot overflow.
    fn step(&mut self, pos: Loc) {
        match pos {
            Loc::Reg(r) => self.a.alu_imm(0, r, 1),
            Loc::Stack(off) => self.a.inc_mem(M(RSP, off)),
        }
    }

    fn set_loc(&mut self, loc: Loc, value: i32) {
        match loc {
            Loc::Reg(r) => self.a.mov_imm64(r, value as i64),
            Loc::Stack(off) => self.a.store_imm(M(RSP, off), value),
        }
    }

    // ---- expressions ----

    /// A value that can be used without computing anything: a small literal or a variable.
    fn simple(&self, e: &Expr) -> Option<(Opnd, Ty)> {
        match &e.kind {
            ExprKind::Int(n) => i32::try_from(*n).ok().map(|n| (Opnd::Imm(n), Ty::Int)),
            ExprKind::Bool(b) => Some((Opnd::Imm(*b as i32), Ty::Bool)),
            ExprKind::Ident(name) => {
                let local = self.scopes.iter().rev().find_map(|s| s.get(name.as_str()))?;
                Some(match local.loc {
                    Loc::Reg(r) => (Opnd::Reg(r), local.ty),
                    Loc::Stack(off) => (Opnd::Mem(M(RSP, off)), local.ty),
                })
            }
            _ => None,
        }
    }

    /// With the left operand in `left` (a register this expression owns),
    /// evaluates the right operand. Returns both operands and what to release.
    fn operands(&mut self, left: u8, rhs: &Expr) -> R<(Opnd, Opnd, Ty, Option<u8>, bool)> {
        if let Some((b, ty)) = self.simple(rhs) {
            return Ok((Opnd::Reg(left), b, ty, None, false));
        }
        if self.has_call(rhs) || self.free.is_empty() {
            // Park the left operand in the frame and reuse its register.
            let slot = self.push_spill();
            self.a.store(slot, left);
            let ty = self.eval(rhs, left)?;
            return Ok((Opnd::Mem(slot), Opnd::Reg(left), ty, None, true));
        }
        let t = self.take_temp(rhs.span)?;
        let ty = self.eval(rhs, t)?;
        Ok((Opnd::Reg(left), Opnd::Reg(t), ty, Some(t), false))
    }

    fn release(&mut self, temp: Option<u8>, spilled: bool) {
        if let Some(t) = temp {
            self.free.push(t);
        }
        if spilled {
            self.spill_depth -= 1;
        }
    }

    /// Evaluates `e` into `dst`, one of r8-r11 or a variable's register.
    fn eval(&mut self, e: &Expr, dst: u8) -> R<Ty> {
        match &e.kind {
            ExprKind::Int(n) => {
                self.a.mov_imm64(dst, *n);
                Ok(Ty::Int)
            }
            ExprKind::Bool(b) => {
                self.a.mov_imm64(dst, *b as i64);
                Ok(Ty::Bool)
            }
            ExprKind::Unit => Ok(Ty::Unit),
            ExprKind::Ident(name) => {
                let local = self.lookup(name, e.span)?;
                self.load_loc(dst, local.loc);
                self.retain(dst, local.ty);
                Ok(local.ty)
            }
            ExprKind::Str(parts) => self.string(parts, dst),
            ExprKind::List(items) => {
                self.a.mov_imm64(RCX, items.len() as i64);
                self.call_rt("list_new");
                let slot = self.push_spill();
                self.a.store(slot, RAX);
                for (i, item) in items.iter().enumerate() {
                    let ty = self.eval(item, dst)?;
                    self.expect_int(ty, item.span)?;
                    self.a.load(RAX, slot);
                    self.a.store(M(RAX, ELEMENTS + 8 * i as i32), dst);
                }
                self.a.load(dst, slot);
                self.spill_depth -= 1;
                self.a.store_imm(M(dst, LENGTH), items.len() as i32);
                Ok(Ty::List)
            }
            ExprKind::Index(list, index) => {
                if let Some(local) = self.list_var(list) {
                    // Read in place: the variable keeps the list alive.
                    if self.eval(index, dst)? != Ty::Int {
                        return unsupported("an index that is not an Int", index.span);
                    }
                    let l = self.reg(local.loc, RCX);
                    let stub = self.bounds_stub(l, dst, e.span);
                    self.a.alu_m(0x3B, dst, M(l, LENGTH));
                    // Unsigned, so that a negative index is out of bounds too.
                    self.a.jcc(CC_AE, stub);
                    self.element(0x8B, dst, l, dst);
                    return Ok(Ty::Int);
                }
                self.owned_list(list, dst)?;
                let slot = self.push_spill();
                self.a.store(slot, dst);
                if self.eval(index, dst)? != Ty::Int {
                    return unsupported("an index that is not an Int", index.span);
                }
                self.a.load(RCX, slot);
                self.spill_depth -= 1;
                let stub = self.bounds_stub(RCX, dst, e.span);
                self.a.alu_m(0x3B, dst, M(RCX, LENGTH));
                self.a.jcc(CC_AE, stub);
                self.element(0x8B, dst, RCX, dst);
                self.release_keeping(dst);
                Ok(Ty::Int)
            }
            ExprKind::MethodCall { recv, name, name_span, args } => self.method(recv, name, *name_span, args, dst),
            ExprKind::Unary(UnOp::Neg, x) => {
                if self.eval(x, dst)? != Ty::Int {
                    return unsupported("negating anything but an Int", e.span);
                }
                let stub = self.fail_stub(Diagnostic::new("E0503", "integer overflow in negation", e.span));
                self.a.mov_rr(RAX, dst);
                self.a.buf.extend_from_slice(&[0x48, 0xF7, 0xD8]); // neg rax
                self.a.jcc(CC_O, stub);
                self.a.mov_rr(dst, RAX);
                Ok(Ty::Int)
            }
            ExprKind::Unary(UnOp::Not, x) => {
                self.eval(x, dst)?;
                // xor dst, 1
                self.a.buf.extend_from_slice(&[0x48 | (dst >> 3), 0x83, 0xF0 | (dst & 7), 1]);
                Ok(Ty::Bool)
            }
            ExprKind::Binary(op, l, r) => match op {
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => self.arithmetic(*op, l, r, dst, e.span),
                _ => {
                    // A comparison or `&&` / `||` as a value: 1 or 0.
                    let (no, done) = (self.a.label(), self.a.label());
                    self.cond(e, no)?;
                    self.a.mov_imm64(dst, 1);
                    self.a.jmp(done);
                    self.a.bind(no);
                    self.a.mov_imm64(dst, 0);
                    self.a.bind(done);
                    Ok(Ty::Bool)
                }
            },
            ExprKind::Call { name, local: false, args, .. } => self.call(name, args, dst, e.span),
            ExprKind::If { cond, then, els } => {
                let (otherwise, done) = (self.a.label(), self.a.label());
                self.cond(cond, otherwise)?;
                let then_ty = self.block(then, dst)?;
                let Some(els) = els else {
                    self.discard(then_ty, then.span)?;
                    self.a.bind(otherwise);
                    return Ok(Ty::Unit);
                };
                self.a.jmp(done);
                self.a.bind(otherwise);
                let else_ty = self.eval(els, dst)?;
                self.a.bind(done);
                Ok(if then_ty == Ty::Never { else_ty } else { then_ty })
            }
            ExprKind::Block(b) => self.block(b, dst),
            ExprKind::Return(value) => {
                let mut keep = None;
                if let Some(v) = value {
                    let ty = self.eval(v, dst)?;
                    if self.ret != Ty::Unit && ty != Ty::Never {
                        self.a.mov_rr(RAX, dst);
                        keep = Some(RAX);
                    }
                }
                // The parameters (scope 0) are released at the common exit.
                self.release_scopes(1, keep);
                self.a.jmp(self.epilogue);
                Ok(Ty::Never)
            }
            ExprKind::Break | ExprKind::Continue => {
                let Some((exit, next, outside)) = self.loops.last().copied() else {
                    return unsupported("`break` outside a loop", e.span);
                };
                self.release_scopes(outside, None);
                self.a.jmp(if matches!(e.kind, ExprKind::Break) { exit } else { next });
                Ok(Ty::Never)
            }
            _ => unsupported("this kind of expression", e.span),
        }
    }

    fn arithmetic(&mut self, op: BinOp, l: &Expr, r: &Expr, dst: u8, span: Span) -> R<Ty> {
        if self.eval(l, dst)? != Ty::Int {
            return unsupported("arithmetic on anything but Ints", span);
        }
        let (a, b, ty, temp, spilled) = self.operands(dst, r)?;
        if ty != Ty::Int {
            return unsupported("arithmetic on anything but Ints", span);
        }
        match op {
            BinOp::Add | BinOp::Sub | BinOp::Mul => {
                // Compute in rax so that both operands survive for the error message.
                // The runtime computes addresses and sizes, which are known to fit.
                let stub = if self.in_runtime { None } else { Some(self.overflow_stub(op, a, b, span)) };
                self.load_opnd(RAX, a);
                match b {
                    Opnd::Imm(n) => self.a.int_op_imm(op, n),
                    Opnd::Mem(m) => self.a.int_op_m(op, m),
                    Opnd::Reg(r) => match op {
                        BinOp::Add => self.a.alu_rr(0x01, RAX, r),
                        BinOp::Sub => self.a.alu_rr(0x29, RAX, r),
                        _ => self.a.imul_rr(RAX, r),
                    },
                }
                if let Some(stub) = stub {
                    self.a.jcc(CC_O, stub);
                }
            }
            _ => {
                self.load_opnd(RAX, a);
                match b {
                    // A constant divisor other than 0 and -1 needs no checks.
                    Opnd::Imm(n) if n != 0 && n != -1 => self.a.divide_by(op, n),
                    _ => {
                        let zero = self.fail_stub(
                            Diagnostic::new("E0504", "division by zero", span)
                                .hint("check the divisor first, or add a `requires` clause"),
                        );
                        let overflow = self.overflow_stub(op, a, b, span);
                        let (plain, done) = (self.a.label(), self.a.label());
                        self.load_opnd(RCX, b);
                        self.a.alu_rr(0x85, RCX, RCX); // test rcx, rcx
                        self.a.jcc(CC_Z, zero);
                        self.a.alu_imm(7, RCX, -1);
                        self.a.jcc(CC_NE, plain);
                        // Dividing by -1 is negation, which overflows for the
                        // smallest Int; the remainder is then an error too, and 0 otherwise.
                        self.a.buf.extend_from_slice(&[0x48, 0xF7, 0xD8]); // neg rax
                        self.a.jcc(CC_O, overflow);
                        if op == BinOp::Rem {
                            self.a.mov_imm64(RAX, 0);
                        }
                        self.a.jmp(done);
                        self.a.bind(plain);
                        self.a.divide(op);
                        self.a.bind(done);
                    }
                }
            }
        }
        self.a.mov_rr(dst, RAX);
        self.release(temp, spilled);
        Ok(Ty::Int)
    }

    /// Jumps to `no` unless `e` is true.
    fn cond(&mut self, e: &Expr, no: usize) -> R<()> {
        match &e.kind {
            ExprKind::Binary(BinOp::And, l, r) => {
                self.cond(l, no)?;
                self.cond(r, no)
            }
            ExprKind::Binary(BinOp::Or, l, r) => {
                let (yes, second) = (self.a.label(), self.a.label());
                self.cond(l, second)?;
                self.a.jmp(yes);
                self.a.bind(second);
                self.cond(r, no)?;
                self.a.bind(yes);
                Ok(())
            }
            ExprKind::Unary(UnOp::Not, x) => {
                let yes = self.a.label();
                self.cond(x, yes)?;
                self.a.jmp(no);
                self.a.bind(yes);
                Ok(())
            }
            ExprKind::Binary(op, l, r) if op.is_comparison() => {
                // A variable in a register can be compared where it is.
                let direct = match (self.simple(l), self.simple(r)) {
                    (Some((Opnd::Reg(reg), lt)), Some((b, rt))) => Some((reg, b, lt, rt)),
                    _ => None,
                };
                // Strings are compared by the runtime: 1 or 0 for equality, and
                // negative / zero / positive for order.
                let routine = if matches!(op, BinOp::Eq | BinOp::Ne) { "str_eq" } else { "str_cmp" };
                let expected = (routine == "str_eq") as i32;
                if let Some((left, right, Ty::Str, Ty::Str)) = direct {
                    // Two variables: nothing to evaluate or release.
                    self.a.mov_rr(RCX, left);
                    self.load_opnd(RDX, right);
                    self.call_rt(routine);
                    self.a.alu_imm(7, RAX, expected);
                    self.a.jcc(condition(*op) ^ 1, no);
                    return Ok(());
                }
                let (left, b, temp, spilled, owned, lt, rt) = match direct {
                    Some((reg, b, lt, rt)) => (Opnd::Reg(reg), b, None, false, None, lt, rt),
                    None => {
                        let t = self.take_temp(e.span)?;
                        let lt = self.eval(l, t)?;
                        if lt == Ty::Str {
                            let slot = self.push_spill();
                            self.a.store(slot, t);
                            self.str_op(routine, (slot, true), Some(r), t)?;
                            self.free.push(t);
                            self.a.alu_imm(7, RAX, expected);
                            self.a.jcc(condition(*op) ^ 1, no);
                            return Ok(());
                        }
                        let (a, b, rt, temp, spilled) = self.operands(t, r)?;
                        (a, b, temp, spilled, Some(t), lt, rt)
                    }
                };
                if lt != rt || !matches!(lt, Ty::Int | Ty::Bool) {
                    return unsupported("comparing anything but Ints, Bools and Strings", e.span);
                }
                self.load_opnd(RAX, left);
                match b {
                    Opnd::Imm(n) => self.a.alu_imm(7, RAX, n),
                    Opnd::Reg(r) => self.a.alu_rr(0x39, RAX, r),
                    Opnd::Mem(m) => self.a.alu_m(0x3B, RAX, m),
                }
                self.a.jcc(condition(*op) ^ 1, no);
                self.release(temp, spilled);
                if let Some(t) = owned {
                    self.free.push(t);
                }
                Ok(())
            }
            _ => {
                let t = self.take_temp(e.span)?;
                self.eval(e, t)?;
                self.a.alu_rr(0x85, t, t); // test t, t
                self.a.jcc(CC_Z, no);
                self.free.push(t);
                Ok(())
            }
        }
    }

    fn call(&mut self, name: &str, args: &[Expr], dst: u8, span: Span) -> R<Ty> {
        if name == "print" {
            return self.print(&args[0]);
        }
        if self.in_runtime && name.starts_with("__") && name != "__copy" {
            return self.intrinsic(name, args, dst, span);
        }
        let key = if self.in_runtime { format!("%{name}") } else { name.to_string() };
        if self.in_runtime {
            self.need_rt(name);
        }
        let Some(info) = self.fns.get(&key) else {
            return unsupported(format!("the function `{name}`"), span);
        };
        if info.params.len() != args.len() {
            return unsupported("a call with the wrong number of arguments", span);
        }
        let (label, ret) = (info.label, info.ret);

        // No value in r8-r11 is live here: whoever holds one parked it before
        // evaluating an expression with a call in it. Each argument goes to
        // its own temp; the temps of later arguments serve as scratch.
        let outer = std::mem::take(&mut self.free);
        let mut parked: Vec<Option<M>> = vec![None; args.len()];
        for (i, arg) in args.iter().enumerate() {
            if self.has_call(arg) {
                // The call inside would overwrite the arguments computed so far.
                for j in 0..i {
                    if parked[j].is_none() {
                        let slot = self.push_spill();
                        self.a.store(slot, ARG_TEMPS[j]);
                        parked[j] = Some(slot);
                    }
                }
            }
            self.free = ARG_TEMPS[i + 1..].iter().copied().filter(|r| TEMPS.contains(r)).collect();
            self.eval(arg, ARG_TEMPS[i])?;
        }
        for i in 0..args.len() {
            match parked[i] {
                Some(slot) => self.a.load(ARG_REGS[i], slot),
                None => self.mov(ARG_REGS[i], ARG_TEMPS[i]),
            }
        }
        self.spill_depth -= parked.iter().flatten().count() as i32;
        self.free = outer;
        self.call_label(label);
        if ret != Ty::Unit {
            self.mov(dst, RAX);
        }
        Ok(ret)
    }

    /// Direct access to memory, for `runtime.yui` only.
    fn intrinsic(&mut self, name: &str, args: &[Expr], dst: u8, span: Span) -> R<Ty> {
        match (name, args) {
            ("__load", [address]) => {
                self.eval(address, dst)?;
                self.a.load(dst, M(dst, 0));
                Ok(Ty::Int)
            }
            ("__store", [address, value]) => {
                self.eval(address, dst)?;
                let (a, b, _, temp, spilled) = self.operands(dst, value)?;
                self.load_opnd(RCX, a);
                self.load_opnd(RAX, b);
                self.a.store(M(RCX, 0), RAX);
                self.release(temp, spilled);
                Ok(Ty::Unit)
            }
            ("__load8", [address]) => {
                self.eval(address, dst)?;
                self.a.mov_rr(RAX, dst);
                self.a.buf.extend_from_slice(&[0x0F, 0xB6, 0x00]); // movzx eax, byte [rax]
                self.a.mov_rr(dst, RAX);
                Ok(Ty::Int)
            }
            ("__store8", [address, value]) => {
                self.eval(address, dst)?;
                let (a, b, _, temp, spilled) = self.operands(dst, value)?;
                self.load_opnd(RCX, a);
                self.load_opnd(RAX, b);
                self.a.buf.extend_from_slice(&[0x88, 0x01]); // mov [rcx], al
                self.release(temp, spilled);
                Ok(Ty::Unit)
            }
            ("__alloc", [bytes]) => {
                self.eval(bytes, dst)?;
                self.a.mov_rr(RCX, dst);
                self.call_label(self.rt_alloc);
                self.a.mov_rr(dst, RAX);
                Ok(Ty::Int)
            }
            ("__realloc", [block, bytes]) => {
                self.eval(block, dst)?;
                let (a, b, _, temp, spilled) = self.operands(dst, bytes)?;
                self.load_opnd(RCX, a);
                self.load_opnd(RDX, b);
                self.release(temp, spilled);
                self.call_label(self.rt_realloc);
                self.a.mov_rr(dst, RAX);
                Ok(Ty::Int)
            }
            ("__free", [block]) => {
                self.eval(block, dst)?;
                self.a.mov_rr(RCX, dst);
                self.call_label(self.rt_free);
                Ok(Ty::Unit)
            }
            _ => unsupported(format!("the intrinsic `{name}`"), span),
        }
    }

    /// A string with `{...}` parts: the pieces are added to a new string one by one.
    fn string(&mut self, parts: &[StrPart], dst: u8) -> R<Ty> {
        if parts.iter().all(|p| matches!(p, StrPart::Lit(_))) {
            let text: String = parts.iter().map(|p| if let StrPart::Lit(t) = p { t.as_str() } else { "" }).collect();
            let constant = self.str_constant(&text);
            self.lea_label(dst, constant);
            return Ok(Ty::Str);
        }
        // Enough room for the usual case, so that adding the pieces does not
        // have to move the string: an Int takes at most 20 bytes.
        let room: usize = parts
            .iter()
            .map(|p| match p {
                StrPart::Lit(text) => text.len(),
                StrPart::Expr(value) => match self.type_of(value) {
                    Some(Ty::Int) => 20,
                    Some(Ty::Bool) => 5,
                    _ => 24,
                },
            })
            .sum();
        self.a.mov_imm64(RCX, room as i64);
        self.call_rt("str_new");
        let building = self.push_spill();
        self.a.store(building, RAX);
        for part in parts {
            match part {
                StrPart::Lit(text) if text.is_empty() => continue,
                StrPart::Lit(text) => {
                    let bytes = self.constant(text.as_bytes());
                    self.lea_label(RDX, bytes);
                    self.a.mov_imm64(R8, text.len() as i64);
                    self.a.load(RCX, building);
                    self.call_rt("str_append");
                }
                StrPart::Expr(value) => {
                    let (slot, owned, ty) = self.park(value, dst)?;
                    match ty {
                        Ty::Int => {
                            self.a.load(RCX, building);
                            self.a.load(RDX, slot);
                            self.call_rt("str_append_int");
                        }
                        Ty::Bool => {
                            self.bool_text(slot);
                            self.a.load(RCX, building);
                            self.call_rt("str_append");
                        }
                        Ty::Str => {
                            self.a.load(RCX, building);
                            self.a.load(RDX, slot);
                            self.call_rt("str_append_str");
                            if owned {
                                self.a.store(building, RAX);
                                self.a.load(RCX, slot);
                                self.call_rt("str_release");
                                self.a.load(RAX, building);
                            }
                        }
                        _ => return unsupported("this kind of value inside a string", value.span),
                    }
                    self.spill_depth -= 1;
                }
            }
            self.a.store(building, RAX);
        }
        self.a.load(dst, building);
        self.spill_depth -= 1;
        Ok(Ty::Str)
    }

    /// Puts the text of the Bool in `slot` in rdx and its length in r8.
    fn bool_text(&mut self, slot: M) {
        let (yes, no) = (self.constant(b"true"), self.constant(b"false"));
        let (is_false, done) = (self.a.label(), self.a.label());
        self.a.cmp_imm(slot, 0);
        self.a.jcc(CC_E, is_false);
        self.lea_label(RDX, yes);
        self.a.mov_imm64(R8, 4);
        self.a.jmp(done);
        self.a.bind(is_false);
        self.lea_label(RDX, no);
        self.a.mov_imm64(R8, 5);
        self.a.bind(done);
    }

    /// A method call. Which methods there are depends on the receiver.
    fn method(&mut self, recv: &Expr, name: &str, name_span: Span, args: &[Expr], dst: u8) -> R<Ty> {
        match (self.type_of(recv), name, args) {
            (Some(Ty::List), ..) => self.list_method(recv, name, name_span, args, dst),
            (Some(Ty::Str), "to_string", []) => self.eval(recv, dst),
            (Some(Ty::Int), "to_string", []) => {
                self.eval(recv, dst)?;
                self.a.mov_rr(RCX, dst);
                self.call_rt("str_from_int");
                self.a.mov_rr(dst, RAX);
                Ok(Ty::Str)
            }
            (Some(Ty::Bool), "to_string", []) => {
                let (yes, no) = (self.str_constant("true"), self.str_constant("false"));
                let (is_false, done) = (self.a.label(), self.a.label());
                self.cond(recv, is_false)?;
                self.lea_label(dst, yes);
                self.a.jmp(done);
                self.a.bind(is_false);
                self.lea_label(dst, no);
                self.a.bind(done);
                Ok(Ty::Str)
            }
            (Some(Ty::Str), ..) => {
                let (routine, result) = match (name, args.len()) {
                    ("len", 0) => ("str_chars", Ty::Int),
                    ("is_empty", 0) => ("str_is_empty", Ty::Bool),
                    ("contains", 1) => ("str_contains", Ty::Bool),
                    ("starts_with", 1) => ("str_starts_with", Ty::Bool),
                    ("ends_with", 1) => ("str_ends_with", Ty::Bool),
                    _ => return unsupported(format!("the method `{name}` of a String"), name_span),
                };
                let (slot, owned, _) = self.park(recv, dst)?;
                self.str_op(routine, (slot, owned), args.first(), dst)?;
                self.a.mov_rr(dst, RAX);
                Ok(result)
            }
            _ => unsupported(format!("the method `{name}` here"), name_span),
        }
    }

    /// A method of a list.
    fn list_method(&mut self, recv: &Expr, name: &str, name_span: Span, args: &[Expr], dst: u8) -> R<Ty> {
        match (name, args) {
            ("len", []) => {
                if let Some(local) = self.list_var(recv) {
                    let l = self.reg(local.loc, RCX);
                    self.a.load(dst, M(l, LENGTH));
                } else {
                    self.owned_list(recv, dst)?;
                    self.a.mov_rr(RCX, dst);
                    self.a.load(dst, M(RCX, LENGTH));
                    self.release_keeping(dst);
                }
                Ok(Ty::Int)
            }
            ("push", [item]) => {
                self.owned_list(recv, dst)?;
                let slot = self.push_spill();
                self.a.store(slot, dst);
                let ty = self.eval(item, dst)?;
                self.expect_int(ty, item.span)?;
                self.a.mov_rr(RDX, dst);
                self.a.load(RCX, slot);
                self.spill_depth -= 1;
                self.call_rt("list_push");
                self.a.mov_rr(dst, RAX);
                Ok(Ty::List)
            }
            ("map" | "filter" | "any" | "all", [f]) => self.list_loop(recv, name, name_span, None, f, dst),
            ("fold", [start, f]) => self.list_loop(recv, name, name_span, Some(start), f, dst),
            _ => unsupported(format!("the method `{name}`"), name_span),
        }
    }

    /// `recv.map(|x| ...)` and its relatives, as a loop with the closure's body inside.
    fn list_loop(&mut self, recv: &Expr, name: &str, name_span: Span, start: Option<&Expr>, f: &Expr, dst: u8) -> R<Ty> {
        let arity = 1 + start.is_some() as usize;
        let (params, body) = match &f.kind {
            ExprKind::Closure { params, body, .. } if params.len() == arity && !crate::bytecode::can_return(body) => {
                (params, body)
            }
            _ => return unsupported("a function passed as a value", f.span),
        };
        let outer = self.fresh_temps(dst);
        let id = name_span.start;
        let (pos, out) = (self.decl_loc[&(id, POS)], self.decl_loc[&(id, OUT)]);
        let item = self.decl_loc[&(params[arity - 1].span.start, VAR)];
        // A list held by a variable is walked where it is: the closure cannot
        // assign to that variable. Any other list is this loop's to release.
        let (list, owned) = match self.list_var(recv) {
            Some(local) => (local.loc, false),
            None => {
                let list = self.decl_loc[&(id, LIST)];
                if self.eval_into(recv, list, true)? != Ty::List {
                    return unsupported(format!("`{name}` on anything but a list of Ints"), recv.span);
                }
                (list, true)
            }
        };
        let mut scope = HashMap::from([(params[arity - 1].name.clone(), Local { ty: Ty::Int, loc: item })]);
        let result = match (name, start) {
            ("fold", Some(start)) => {
                let ty = self.eval_into(start, out, false)?;
                if !matches!(ty, Ty::Int | Ty::Bool) {
                    return unsupported("`fold` to anything but an Int or a Bool", start.span);
                }
                scope.insert(params[0].name.clone(), Local { ty, loc: out });
                ty
            }
            ("map" | "filter", _) => {
                // Room for every item, so that the loop never has to grow the result.
                let l = self.reg(list, RAX);
                self.a.load(RCX, M(l, LENGTH));
                self.call_rt("list_new");
                self.store_loc(out, RAX);
                Ty::List
            }
            _ => {
                self.set_loc(out, (name == "all") as i32);
                Ty::Bool
            }
        };
        self.set_loc(pos, 0);
        let (top, next, exit) = (self.a.label(), self.a.label(), self.a.label());
        self.a.bind(top);
        self.next_item(list, pos, item, exit);
        self.scopes.push(scope);
        match name {
            "map" | "fold" => {
                let t = self.take_temp(body.span)?;
                let ty = self.eval(body, t)?;
                if ty != (if name == "map" { Ty::Int } else { result }) {
                    return unsupported("this closure", body.span);
                }
                if name == "map" {
                    let o = self.reg(out, RCX);
                    let p = self.reg(pos, RAX);
                    self.element(0x89, t, o, p);
                } else {
                    self.store_loc(out, t);
                }
                self.free.push(t);
            }
            // The rest only branch on what the closure says.
            "filter" => {
                self.cond(body, next)?;
                let o = self.reg(out, RCX);
                self.a.load(RAX, M(o, LENGTH));
                let x = self.reg(item, RDX);
                self.element(0x89, x, o, RAX);
                self.a.inc_mem(M(o, LENGTH));
            }
            "any" => {
                // Stops at the first true.
                self.cond(body, next)?;
                self.set_loc(out, 1);
                self.a.jmp(exit);
            }
            _ => {
                // `all` stops at the first false.
                let no = self.a.label();
                self.cond(body, no)?;
                self.a.jmp(next);
                self.a.bind(no);
                self.set_loc(out, 0);
                self.a.jmp(exit);
            }
        }
        self.scopes.pop();
        self.a.bind(next);
        self.step(pos);
        self.a.jmp(top);
        self.a.bind(exit);
        if name == "map" {
            let l = self.reg(list, RAX);
            self.a.load(RAX, M(l, LENGTH));
            let o = self.reg(out, RCX);
            self.a.store(M(o, LENGTH), RAX);
        }
        if owned {
            self.load_loc(RCX, list);
            self.call_rt("list_release");
        }
        self.free = outer;
        self.load_loc(dst, out);
        Ok(result)
    }

    /// `print(...)`: a literal is written piece by piece, without building
    /// the string; anything else is a string to write. Then a line break.
    fn print(&mut self, arg: &Expr) -> R<Ty> {
        match &arg.kind {
            ExprKind::Str(parts) => {
                for part in parts {
                    match part {
                        StrPart::Lit(text) if text.is_empty() => {}
                        StrPart::Lit(text) => self.write(text.as_bytes(), STDOUT),
                        StrPart::Expr(value) => self.print_value(value)?,
                    }
                }
            }
            _ => self.print_value(arg)?,
        }
        self.write(b"
", STDOUT);
        Ok(Ty::Unit)
    }

    /// Writes the text of one value.
    fn print_value(&mut self, value: &Expr) -> R<()> {
        let outer = std::mem::replace(&mut self.free, TEMPS[1..].to_vec());
        let t = TEMPS[0];
        // A string held by a variable is written from where it is.
        let borrowed = self.heap_var(value).filter(|local| local.ty == Ty::Str);
        let ty = match borrowed {
            Some(local) => {
                self.load_loc(t, local.loc);
                Ty::Str
            }
            None => self.eval(value, t)?,
        };
        self.free = outer;
        match ty {
            Ty::Int => {
                self.a.mov_rr(RCX, t);
                self.a.mov_imm64(RDX, STDOUT);
                self.call_label(self.rt_print_int);
            }
            Ty::Bool => {
                let slot = self.push_spill();
                self.a.store(slot, t);
                self.bool_text(slot);
                self.spill_depth -= 1;
                self.a.mov_rr(RCX, RDX);
                self.a.mov_rr(RDX, R8);
                self.a.mov_imm64(R8, STDOUT);
                self.call_label(self.rt_write);
            }
            Ty::Str => {
                let slot = self.push_spill();
                self.a.store(slot, t);
                self.a.lea(RCX, M(t, ELEMENTS));
                self.a.load(RDX, M(t, LENGTH));
                self.a.mov_imm64(R8, STDOUT);
                self.call_label(self.rt_write);
                if borrowed.is_none() {
                    self.a.load(RCX, slot);
                    self.call_rt("str_release");
                }
                self.spill_depth -= 1;
            }
            _ => return unsupported("printing this kind of value", value.span),
        }
        Ok(())
    }

    fn write(&mut self, bytes: &[u8], to: i64) {
        let text = self.constant(bytes);
        self.lea_label(RCX, text);
        self.a.mov_imm64(RDX, bytes.len() as i64);
        self.a.mov_imm64(R8, to);
        self.call_label(self.rt_write);
    }
}

/// Whether `e` reads the variable `name`.
fn mentions(e: &Expr, name: &str) -> bool {
    match &e.kind {
        ExprKind::Ident(n) => n == name,
        ExprKind::Int(_) | ExprKind::Bool(_) | ExprKind::Unit => false,
        ExprKind::Unary(_, x) => mentions(x, name),
        ExprKind::Binary(_, l, r) | ExprKind::Index(l, r) => mentions(l, name) || mentions(r, name),
        _ => true,
    }
}

// ---- the executable file ----

fn align(n: usize, to: usize) -> usize {
    n.div_ceil(to) * to
}

/// Wraps machine code in a Windows executable (PE32+): headers, one section
/// of code and constants, and one section with the import table.
/// `iat_fixups` are the places in the code that call imported functions.
/// `bss_fixups` are the places that refer to writable memory; if there are
/// any, a third section provides a page of it, zeroed by the loader.
fn write_pe(mut code: Vec<u8>, iat_fixups: &[(usize, usize)], bss_fixups: &[(usize, usize)]) -> Vec<u8> {
    const FILE_ALIGN: usize = 0x200;
    const SECTION_ALIGN: usize = 0x1000;
    const IMAGE_BASE: u64 = 0x1_4000_0000;
    const HEADERS: usize = 0x200;
    let text_rva = SECTION_ALIGN;
    let idata_rva = text_rva + align(code.len().max(1), SECTION_ALIGN);

    // The import section: directory, lookup table, address table, names.
    let count = IMPORTS.len();
    let ilt = 40;
    let iat = ilt + 8 * (count + 1);
    let mut names = iat + 8 * (count + 1);
    let mut idata = vec![0u8; names];
    let mut entries = Vec::new();
    for name in IMPORTS {
        entries.push(idata_rva + names);
        idata.extend([0, 0]);
        idata.extend(name.as_bytes());
        idata.push(0);
        if idata.len() % 2 == 1 {
            idata.push(0);
        }
        names = idata.len();
    }
    let dll_name = idata_rva + idata.len();
    idata.extend(b"KERNEL32.dll\0");
    for (i, entry) in entries.iter().enumerate() {
        idata[ilt + 8 * i..ilt + 8 * i + 8].copy_from_slice(&(*entry as u64).to_le_bytes());
        idata[iat + 8 * i..iat + 8 * i + 8].copy_from_slice(&(*entry as u64).to_le_bytes());
    }
    idata[0..4].copy_from_slice(&((idata_rva + ilt) as u32).to_le_bytes());
    idata[12..16].copy_from_slice(&(dll_name as u32).to_le_bytes());
    idata[16..20].copy_from_slice(&((idata_rva + iat) as u32).to_le_bytes());

    // Calls to imports are relative to the instruction that follows them.
    for (at, import) in iat_fixups {
        let target = (idata_rva + iat + 8 * import) as i64;
        let rel = (target - (text_rva + at + 4) as i64) as i32;
        code[*at..*at + 4].copy_from_slice(&rel.to_le_bytes());
    }

    let bss_rva = idata_rva + align(idata.len(), SECTION_ALIGN);
    let bss_size = if bss_fixups.is_empty() { 0 } else { SECTION_ALIGN };
    for (at, offset) in bss_fixups {
        let rel = ((bss_rva + offset) as i64 - (text_rva + at + 4) as i64) as i32;
        code[*at..*at + 4].copy_from_slice(&rel.to_le_bytes());
    }

    let text_raw = align(code.len(), FILE_ALIGN);
    let idata_raw = align(idata.len(), FILE_ALIGN);
    let image_size = bss_rva + bss_size;

    let mut out = Vec::new();
    let u16le = |out: &mut Vec<u8>, v: u16| out.extend(v.to_le_bytes());
    let u32le = |out: &mut Vec<u8>, v: usize| out.extend((v as u32).to_le_bytes());
    let u64le = |out: &mut Vec<u8>, v: u64| out.extend(v.to_le_bytes());

    // DOS header: only the signature and the offset of the PE header matter.
    out.extend(b"MZ");
    out.resize(0x3C, 0);
    u32le(&mut out, 0x40);
    out.extend(b"PE\0\0");
    // COFF header.
    u16le(&mut out, 0x8664); // x64
    u16le(&mut out, 2 + (bss_size != 0) as u16); // sections
    u32le(&mut out, 0);
    u32le(&mut out, 0);
    u32le(&mut out, 0);
    u16le(&mut out, 240); // size of the optional header
    u16le(&mut out, 0x0023); // executable, no relocations, large addresses
    // Optional header (PE32+).
    u16le(&mut out, 0x20B);
    u16le(&mut out, 0);
    u32le(&mut out, text_raw);
    u32le(&mut out, idata_raw);
    u32le(&mut out, 0);
    u32le(&mut out, text_rva); // entry point: the first byte of the code
    u32le(&mut out, text_rva);
    u64le(&mut out, IMAGE_BASE);
    u32le(&mut out, SECTION_ALIGN);
    u32le(&mut out, FILE_ALIGN);
    u16le(&mut out, 6); // operating system version 6.0
    u16le(&mut out, 0);
    u16le(&mut out, 0);
    u16le(&mut out, 0);
    u16le(&mut out, 6); // subsystem version 6.0
    u16le(&mut out, 0);
    u32le(&mut out, 0);
    u32le(&mut out, image_size);
    u32le(&mut out, HEADERS);
    u32le(&mut out, 0);
    u16le(&mut out, 3); // console program
    u16le(&mut out, 0x8100); // no-execute compatible, terminal server aware
    u64le(&mut out, 0x80_0000); // stack reserve: 8 MB
    u64le(&mut out, 0x1000);
    u64le(&mut out, 0x10_0000);
    u64le(&mut out, 0x1000);
    u32le(&mut out, 0);
    u32le(&mut out, 16); // data directories
    for directory in 0..16 {
        let (rva, size) = match directory {
            1 => (idata_rva, 40),
            12 => (idata_rva + iat, 8 * (count + 1)),
            _ => (0, 0),
        };
        u32le(&mut out, rva);
        u32le(&mut out, size);
    }
    // Section headers.
    let section = |out: &mut Vec<u8>, name: &[u8; 8], size: usize, rva: usize, raw: usize, at: usize, flags: u32| {
        out.extend(name);
        u32le(out, size);
        u32le(out, rva);
        u32le(out, raw);
        u32le(out, at);
        out.extend([0u8; 12]);
        out.extend(flags.to_le_bytes());
    };
    section(&mut out, b".text\0\0\0", code.len(), text_rva, text_raw, HEADERS, 0x6000_0020);
    section(&mut out, b".idata\0\0", idata.len(), idata_rva, idata_raw, HEADERS + text_raw, 0xC000_0040);
    if bss_size != 0 {
        section(&mut out, b".bss\0\0\0\0", bss_size, bss_rva, 0, 0, 0xC000_0080);
    }
    out.resize(HEADERS, 0);
    out.extend(&code);
    out.resize(HEADERS + text_raw, 0);
    out.extend(&idata);
    out.resize(HEADERS + text_raw + idata_raw, 0);
    out
}
