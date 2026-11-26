//! Ahead-of-time compiler: turns a program into a Windows x64 executable that
//! contains no interpreter (see `docs/native-backend.md`).
//!
//! It handles programs made of functions over `Int`, `Bool`, `String`,
//! lists, structs and enums (with `Option` and `Result`): arithmetic,
//! comparisons, `if`, `while`, `for`, `match`, `?`, calls, list literals,
//! `push`, `len`, `get`, indexing, `map` / `filter` / `fold` / `any` / `all`
//! with a closure written in place, strings with `{...}` parts, comparisons
//! and a few methods on strings, and `print`. Anything else is reported as
//! `Unsupported`.
//!
//! Every value is 8 bytes. Lists, strings, structs and enum values with
//! fields live on the heap with a reference count in front (lists and
//! strings are laid out in `runtime.yui`, which also holds their routines;
//! they are compiled by this same compiler). A struct is `[count][fields]`.
//! An enum value is `[count][variant][fields]`, or, for a variant without
//! fields, just the number of the variant: a number below `SMALL` is never
//! an address. Every variable of such a type owns one reference, and every
//! such expression produces one; a reference is given up when its variable
//! goes out of scope or is assigned to. What a `for`, a `match` pattern or a
//! closure parameter names is borrowed from the list or value it came from,
//! which is kept alive meanwhile. String literals are constants in the
//! executable, marked by a negative count that is never changed.
//!
//! A function value (a closure, or a function used by name) is a counted
//! block `[count][code][how to free it][captured values]`. Its code takes
//! the block in rcx and the arguments after it. Captured values are copied
//! into the block when the closure is made.
//!
//! Generic types are compiled once per combination of type arguments. The
//! code that frees a value of each type (and what it holds) is generated on
//! demand.
//!
//! Values have no tags: an Int is 8 bytes in a register. Each function keeps
//! its busiest variables in registers that calls preserve (rbx, rsi, rdi,
//! r12-r15) and the rest in its stack frame. Expressions are evaluated into
//! r8-r11; rax, rcx and rdx are scratch. Functions follow the Windows x64
//! calling convention, so they can call the operating system directly.

use super::{condition, Asm, CC_A, CC_AE, CC_B, CC_BE, CC_E, CC_G, CC_GE, CC_L, CC_LE, CC_NE, CC_O, M, R10, R11, R12, R13, R14, R15, R8, R9, RAX, RBX, RCX, RDI, RDX, RSI};
use crate::ast::*;
use crate::diag::{render_human, Diagnostic, SourceMap, Span};
use std::collections::HashMap;
use std::rc::Rc;

const RSP: u8 = 4;
const CC_NS: u8 = 0x9;
const CC_Z: u8 = CC_E;
const CC_NZ: u8 = CC_NE;
/// "Parity": set by a comparison of floats when one of them is not a number.
const CC_P: u8 = 0xA;
const CC_NP: u8 = 0xB;

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
/// Functions of the operating system that the generated code may call: the
/// library and the name. An executable imports the ones it actually uses.
const IMPORTS: [(&str, &str); 37] = [
    ("KERNEL32.dll", "GetStdHandle"),
    ("KERNEL32.dll", "WriteFile"),
    ("KERNEL32.dll", "ExitProcess"),
    ("KERNEL32.dll", "HeapAlloc"),
    ("KERNEL32.dll", "HeapFree"),
    ("KERNEL32.dll", "HeapReAlloc"),
    ("KERNEL32.dll", "ReadFile"),
    ("KERNEL32.dll", "GetCommandLineW"),
    ("KERNEL32.dll", "CreateFileW"),
    ("KERNEL32.dll", "GetFileSizeEx"),
    ("KERNEL32.dll", "CloseHandle"),
    ("KERNEL32.dll", "GetLastError"),
    ("KERNEL32.dll", "FormatMessageW"),
    ("KERNEL32.dll", "GetModuleHandleW"),
    ("KERNEL32.dll", "GetTickCount64"),
    ("KERNEL32.dll", "Sleep"),
    ("KERNEL32.dll", "QueryPerformanceCounter"),
    ("KERNEL32.dll", "CreateWaitableTimerExW"),
    ("KERNEL32.dll", "SetWaitableTimer"),
    ("KERNEL32.dll", "WaitForSingleObject"),
    ("USER32.dll", "RegisterClassExW"),
    ("USER32.dll", "CreateWindowExW"),
    ("USER32.dll", "DefWindowProcW"),
    ("USER32.dll", "PeekMessageW"),
    ("USER32.dll", "TranslateMessage"),
    ("USER32.dll", "DispatchMessageW"),
    ("USER32.dll", "IsWindow"),
    ("USER32.dll", "DestroyWindow"),
    ("USER32.dll", "AdjustWindowRect"),
    ("USER32.dll", "GetDC"),
    ("USER32.dll", "ReleaseDC"),
    ("USER32.dll", "GetAsyncKeyState"),
    ("USER32.dll", "GetCursorPos"),
    ("USER32.dll", "ScreenToClient"),
    ("USER32.dll", "LoadCursorW"),
    ("USER32.dll", "GetForegroundWindow"),
    ("GDI32.dll", "StretchDIBits"),
];
/// The imports `runtime.yui` may call as `__os_<name>(...)`, with how many
/// arguments each takes. They follow the same calling convention as the
/// code this compiler makes, so a call is an ordinary call. (The address of
/// any import can also be had with `__import_<name>()`.)
const OS_CALLS: [(&str, usize); 32] = [
    ("GetStdHandle", 1),
    ("WriteFile", 5),
    ("ReadFile", 5),
    ("GetCommandLineW", 0),
    ("CreateFileW", 7),
    ("GetFileSizeEx", 2),
    ("CloseHandle", 1),
    ("GetLastError", 0),
    ("FormatMessageW", 7),
    ("GetModuleHandleW", 1),
    ("GetTickCount64", 0),
    ("Sleep", 1),
    ("QueryPerformanceCounter", 1),
    ("CreateWaitableTimerExW", 4),
    ("SetWaitableTimer", 6),
    ("WaitForSingleObject", 2),
    ("RegisterClassExW", 1),
    ("CreateWindowExW", 12),
    ("PeekMessageW", 5),
    ("TranslateMessage", 1),
    ("DispatchMessageW", 1),
    ("IsWindow", 1),
    ("DestroyWindow", 1),
    ("AdjustWindowRect", 3),
    ("GetDC", 1),
    ("ReleaseDC", 2),
    ("GetAsyncKeyState", 1),
    ("GetCursorPos", 1),
    ("ScreenToClient", 2),
    ("LoadCursorW", 2),
    ("GetForegroundWindow", 0),
    ("StretchDIBits", 13),
];
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
/// The value a `match` looks at.
const SCRUT: u8 = 5;
/// The function a loop over a list calls for each item.
const FUNC: u8 = 6;
/// A variable a closure captured, by its position in the closure's block.
const CAPTURE: u8 = 7;
/// Where a function with contracts keeps what it was told about its caller,
/// and its result while the `ensures` clauses look at it.
const SITE: u8 = 8;
const RESULT: u8 = 9;

/// Offsets in the block of a function value.
const FN_CODE: i32 = 8;
const FN_FREE: i32 = 16;
const FN_CAPTURES: i32 = 24;

/// A map is laid out like a list whose items are pairs: after the count, the
/// length and the capacity come the entries, 16 bytes each, key then value.
const MAP_KEY: u8 = 24;
const MAP_VALUE: u8 = 32;
/// Kinds of generated code per map type.
const MAP_SET: u8 = 0;
const MAP_REMOVE: u8 = 1;

/// Enum values below this are variants without fields, not addresses.
const SMALL: i32 = 4096;

/// Writable memory (zero at start): the number of heap blocks in use, kept
/// only when the executable is built to check for leaks.
const LIVE_BLOCKS: usize = 0;
/// The small-block allocator: for each size (a multiple of 8, up to
/// `POOL_MAX`), at `POOL_HEADS + size`, the first of a chain of freed blocks
/// of that size; and the unused rest of the memory it took from the system.
const POOL_HEADS: usize = 8;
const POOL_NEXT: usize = 88;
const POOL_END: usize = 96;
/// Cells the runtime keeps things in between calls (`__global(i)`): the
/// window, its pixels and size, the state of the random numbers.
const GLOBALS: usize = 104;
const GLOBAL_SCALE: usize = 4;
/// Where the rest of the writable memory starts: tables of remembered results.
const BSS_FIXED: usize = GLOBALS + 8 * 8;
/// How many results a function remembers.
const MEMO_ENTRIES: usize = 1024;
/// Blocks up to this size come from the small-block allocator.
const POOL_MAX: usize = 64;
/// How much it takes from the system at a time.
const POOL_CHUNK: u32 = 1 << 16;

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
    /// The 8 bytes of a 64-bit float, kept in the same registers as an Int.
    Float,
    Bool,
    Unit,
    /// A list: a pointer to a counted block on the heap. The number says which list type.
    List(u32),
    /// `String`: a pointer to a counted block on the heap, or to a constant.
    Str,
    /// A struct: a pointer to a counted block on the heap.
    Struct(u32),
    /// An enum: the number of a variant without fields, or a pointer to a counted block.
    Enum(u32),
    /// A function value: a pointer to a counted block.
    Fn(u32),
    /// A map: a pointer to a counted block of its entries, in the order of the keys.
    Map(u32),
    /// The expression does not produce a value (`return`, `break`, ...).
    Never,
}

impl Ty {
    /// Whether values of this type are counted references.
    fn heap(self) -> bool {
        matches!(self, Ty::List(_) | Ty::Str | Ty::Struct(_) | Ty::Enum(_) | Ty::Fn(_) | Ty::Map(_))
    }
}

/// What a numbered type is made of.
#[derive(Clone)]
enum Def {
    /// The type of the items.
    List(Ty),
    /// The fields, in the order of the declaration.
    Struct(Vec<(String, Ty)>),
    /// The variants, in the order of the declaration, with the types of their fields.
    Enum(Vec<(String, Vec<Ty>)>),
    /// The types of the parameters and of the result.
    Fn(Vec<Ty>, Ty),
    /// The types of the keys and of the values.
    Map(Ty, Ty),
}

/// A closure whose body is still to be compiled.
struct Closure {
    code: usize,
    /// Code that frees the closure's block.
    free: usize,
    bytes: usize,
    /// Name, where it is declared, and type.
    params: Vec<(String, usize, Ty)>,
    captures: Vec<(String, Ty)>,
    ret: Ty,
    body: std::rc::Rc<Expr>,
    /// What the type parameters of the function it was written in stand for.
    env: Vec<(String, Ty)>,
}

/// What a function to compile is made of.
enum Body<'b> {
    Block(&'b Block),
    Expr(&'b Expr),
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
    /// The value belongs to something else that outlives this variable, so
    /// the variable does not release it.
    borrowed: bool,
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
    /// Has `requires` or `ensures`: a caller says where it is (see `site`).
    contracts: bool,
}

/// A piece of an error message that is put together when the error happens.
#[derive(Clone, Copy)]
enum Part {
    /// Fixed text: its label and length.
    Text(usize, usize),
    /// A value parked in a spill slot, written the way it appears inside other values.
    Value(M, Ty),
    /// A string parked in a spill slot, written as it is.
    Raw(M),
}

/// Error-reporting code that belongs to a function but is kept out of its body.
enum Stub {
    Fail { label: usize, message: usize, len: usize },
    Overflow { label: usize, a: Opnd, b: Opnd, record: usize },
    Bounds { label: usize, list: u8, index: u8, record: usize },
    /// A key that is not in a map. The message is `before`, the key (parked
    /// in `key`) as text, and `after`: (label of the text, its length) each.
    Missing { label: usize, key: M, ty: Ty, before: (usize, usize), after: (usize, usize) },
    /// An error whose message is the parts in order; the last one is text.
    Report { label: usize, parts: Vec<Part> },
    /// An error whose message has one number in it, the value of a register.
    Number { label: usize, value: u8, record: usize },
    /// A violated `requires` or `ensures`. `header` is the first line of the
    /// message (up to the result, for `ensures`); the rest names the place
    /// of the call, which the caller passed and the function kept at `site`.
    Contract { label: usize, header: usize, len: usize, site: Loc, result: Option<(Ty, Loc)> },
    /// `variable = variable.push(value)` when the item cannot be added in place.
    Push { label: usize, back: usize, variable: Loc, value: u8, list: Ty },
    /// A store into `variable` (through the fields at the offsets in
    /// `fields`, with the type each holds) when something on the way is
    /// shared: the variable gets copies of its own first. `saves` are spill
    /// slots for `a`, `b` and the value being walked meanwhile.
    Unshare { label: usize, back: usize, variable: Loc, root: Ty, fields: Vec<(i32, Ty)>, a: Opnd, b: Opnd, saves: [M; 3] },
}

struct Native<'a> {
    a: Asm,
    sources: &'a SourceMap,
    types: &'a TypeInfo,
    /// The program's functions by name; the runtime's have a `%` in front.
    fns: HashMap<String, FnInfo>,
    structs: HashMap<String, &'a StructDecl>,
    enums: HashMap<String, &'a EnumDecl>,
    /// Functions with type parameters. Each is compiled once per combination
    /// of types it is used with; `instances` are the ones still to compile:
    /// the name in `fns`, the function, and what its type parameters stand for.
    generics: HashMap<String, Rc<FnDecl>>,
    instances: Vec<(String, Rc<FnDecl>, Vec<(String, Ty)>)>,
    /// What the type parameters of the function being compiled stand for.
    env: Vec<(String, Ty)>,
    /// The numbered types, the type each name stands for (`Option<Int>`), and the names.
    defs: Vec<Def>,
    def_ids: HashMap<String, Ty>,
    def_keys: Vec<String>,
    /// Generated code per type: giving up a reference, and `push` for lists
    /// whose items are counted. The queue holds what is not emitted yet.
    drop_labels: HashMap<u32, usize>,
    push_labels: HashMap<u32, usize>,
    glue_queue: Vec<(bool, u32)>,
    /// Generated code per type that copies a list or a struct, for a
    /// variable that is about to change a value it shares.
    clone_labels: HashMap<u32, usize>,
    clone_queue: Vec<u32>,
    /// Generated code per type that compares two values, and that writes a
    /// value out as text. Queue entries: (true for text, the type).
    eq_labels: HashMap<u32, usize>,
    repr_labels: HashMap<u32, usize>,
    text_queue: Vec<(bool, u32)>,
    /// Generated code per map type: `set` and `remove` (see `MAP_SET`).
    map_labels: HashMap<(u8, u32), usize>,
    map_queue: Vec<(u8, u32)>,
    rt_number: usize,
    rt_contract: usize,
    /// For each of `OS_CALLS`: the label of a jump to the import.
    os_labels: Vec<usize>,
    /// Whether those two are needed.
    uses_errors: bool,
    /// Closures to compile, and for each function used as a value, the code
    /// that calls it the way a closure is called: (label, the function).
    closures: Vec<Closure>,
    thunks: HashMap<String, usize>,
    thunk_queue: Vec<(usize, usize)>,
    /// Frees the block of a function value that captured nothing.
    plain_free: Option<usize>,
    /// Positions of `call [rip + ...]` displacements and the import they call.
    iat_fixups: Vec<(usize, usize)>,
    /// Constant data, placed after the code.
    data: Vec<(usize, Vec<u8>)>,
    /// Positions of rip-relative displacements into the writable memory, and the offset there.
    bss_fixups: Vec<(usize, usize)>,
    /// How much writable memory is in use.
    bss_used: usize,
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
    rt_pool_alloc: usize,
    rt_pool_free: usize,
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
    /// scopes outside the loop body, number of held values outside it)
    loops: Vec<(usize, usize, usize, usize)>,
    /// Counted values that an expression being compiled holds in spill slots
    /// while it evaluates its other parts. If one of those parts leaves the
    /// function or the loop (`?`, `return`, `break`), they are released there.
    pending: Vec<(M, Ty)>,
    /// A variable whose next read is its last, so that the read takes over
    /// the variable's reference instead of adding one. With `true` the
    /// variable is then treated as borrowed (it no longer owns anything);
    /// with `false` whoever set this gives the variable a new value.
    moving: Option<(String, bool)>,
    /// Whether that read happened.
    moved: bool,
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
    let mut a = Asm::default();
    let mut fns = HashMap::new();
    let Ok(runtime) = crate::parser::parse_file(RUNTIME, 0, "", true) else {
        return unsupported("its own runtime (it does not parse)", nowhere);
    };
    let mut runtime_fns = Vec::new();
    for item in &runtime.items {
        if let Item::Fn(f) = item {
            let ret = if f.ret.is_some() { Ty::Int } else { Ty::Unit };
            fns.insert(format!("%{}", f.name), FnInfo { label: a.label(), params: vec![Ty::Int; f.params.len()], ret, contracts: false });
            runtime_fns.push(f.clone());
        }
    }
    // Copying memory is machine code, but is called like a function of the runtime.
    let rt_copy = a.label();
    fns.insert("%__copy".to_string(), FnInfo { label: rt_copy, params: vec![Ty::Int; 3], ret: Ty::Unit, contracts: false });

    let (rt_write, rt_print_int, rt_fail, rt_overflow) = (a.label(), a.label(), a.label(), a.label());
    let (rt_alloc, rt_realloc, rt_free) = (a.label(), a.label(), a.label());
    let (rt_pool_alloc, rt_pool_free) = (a.label(), a.label());
    let (rt_number, rt_contract) = (a.label(), a.label());
    let mut os_labels = Vec::new();
    for (name, params) in OS_CALLS {
        let label = a.label();
        os_labels.push(label);
        let info = FnInfo { label, params: vec![Ty::Int; params], ret: Ty::Int, contracts: false };
        fns.insert(format!("%__os_{name}"), info);
    }
    let mut n = Native {
        a,
        sources,
        types: &program.types,
        fns,
        structs: HashMap::new(),
        enums: HashMap::new(),
        generics: HashMap::new(),
        instances: Vec::new(),
        env: Vec::new(),
        defs: Vec::new(),
        def_ids: HashMap::new(),
        def_keys: Vec::new(),
        drop_labels: HashMap::new(),
        push_labels: HashMap::new(),
        glue_queue: Vec::new(),
        clone_labels: HashMap::new(),
        clone_queue: Vec::new(),
        eq_labels: HashMap::new(),
        repr_labels: HashMap::new(),
        text_queue: Vec::new(),
        map_labels: HashMap::new(),
        map_queue: Vec::new(),
        rt_number,
        rt_contract,
        os_labels,
        uses_errors: false,
        closures: Vec::new(),
        thunks: HashMap::new(),
        thunk_queue: Vec::new(),
        plain_free: None,
        iat_fixups: Vec::new(),
        data: Vec::new(),
        bss_fixups: Vec::new(),
        bss_used: BSS_FIXED,
        uses_heap: false,
        rt_seen: Default::default(),
        rt_queue: Vec::new(),
        rt_copy,
        in_runtime: false,
        check_heap: std::env::var_os("YUI_NATIVE_CHECK_HEAP").is_some(),
        rt_alloc,
        rt_realloc,
        rt_free,
        rt_pool_alloc,
        rt_pool_free,
        rt_write,
        rt_print_int,
        rt_fail,
        rt_overflow,
        stubs: Vec::new(),
        scopes: Vec::new(),
        decl_loc: HashMap::new(),
        free: Vec::new(),
        loops: Vec::new(),
        pending: Vec::new(),
        moving: None,
        moved: false,
        epilogue: 0,
        ret: Ty::Unit,
        spill_base: 0,
        spill_depth: 0,
        spill_max: 0,
    };
    for item in &program.items {
        match item {
            Item::Struct(decl) => {
                n.structs.insert(decl.name.clone(), decl);
            }
            Item::Enum(decl) => {
                n.enums.insert(decl.name.clone(), decl);
            }
            _ => {}
        }
    }
    // Only the functions that `main` reaches are compiled: the rest (what
    // only tests use, for one) does not have to be supported, or carried.
    let by_name: HashMap<&str, &Rc<FnDecl>> = program
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Fn(f) => Some((f.name.as_str(), f)),
            _ => None,
        })
        .collect();
    let mut reached: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut todo = vec!["main"];
    while let Some(name) = todo.pop() {
        let Some(f) = by_name.get(name) else { continue };
        if !reached.insert(f.name.as_str()) {
            continue;
        }
        let mut note = |e: &Expr| {
            match &e.kind {
                ExprKind::Call { name, local: false, .. } | ExprKind::FnRef(name) => {
                    if let Some(callee) = by_name.get(name.as_str()) {
                        todo.push(callee.name.as_str());
                    }
                }
                _ => {}
            }
            false
        };
        any_block(&f.body, &mut note);
        for clause in f.requires.iter().chain(&f.ensures) {
            any_expr(clause, &mut note);
        }
    }
    let mut decls = Vec::new();
    for item in &program.items {
        let Item::Fn(f) = item else { continue };
        if !reached.contains(f.name.as_str()) {
            continue;
        }
        if !f.type_params.is_empty() {
            // Compiled when used, once per combination of types.
            n.generics.insert(f.name.clone(), f.clone());
            continue;
        }
        let mut params = Vec::new();
        for p in &f.params {
            params.push(n.ty_expr(&p.ty)?);
        }
        let ret = match &f.ret {
            Some(t) => n.ty_expr(t)?,
            None => Ty::Unit,
        };
        let label = n.a.label();
        let contracts = !f.requires.is_empty() || !f.ensures.is_empty();
        n.fns.insert(f.name.clone(), FnInfo { label, params, ret, contracts });
        decls.push(f.clone());
    }
    let Some(main) = n.fns.get("main") else { return unsupported("a program without `main`", nowhere) };
    let main_label = main.label;

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
    // Whatever the functions needed: closures (which may need more in turn),
    // and code generated per type.
    loop {
        if let Some((key, f, env)) = n.instances.pop() {
            n.env = env;
            n.function(&f, &key)?;
        } else if let Some(closure) = n.closures.pop() {
            n.env = closure.env.clone();
            n.closure(closure)?;
        } else if let Some((push, id)) = n.glue_queue.pop() {
            if push {
                n.emit_push(id);
            } else {
                n.emit_drop(id);
            }
        } else if let Some(id) = n.clone_queue.pop() {
            n.emit_clone(id);
        } else if let Some((kind, id)) = n.map_queue.pop() {
            n.emit_map(kind, id);
        } else if let Some((text, id)) = n.text_queue.pop() {
            if text {
                n.emit_repr(id);
            } else {
                n.emit_eq(id);
            }
        } else if let Some((label, target)) = n.thunk_queue.pop() {
            // The block comes first; the arguments move up by one.
            n.a.bind(label);
            n.a.mov_rr(RCX, RDX);
            n.a.mov_rr(RDX, R8);
            n.a.mov_rr(R8, R9);
            n.a.jmp(target);
        } else {
            break;
        }
    }
    if n.uses_errors {
        n.error_runtime();
    }
    if let Some(label) = n.plain_free {
        n.a.bind(label);
        n.a.mov_imm64(RDX, FN_CAPTURES as i64);
        n.a.jmp(n.rt_pool_free);
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
    Ok(write_pe(n.a.finish(), &iat_fixups, &bss_fixups, n.bss_used))
}

type Decls = Vec<(usize, String, u8)>;

/// Registers a hidden value of the loop at `id`, used every round.
fn hidden(id: usize, kind: u8, depth: u32, weight: &mut HashMap<String, usize>, decls: &mut Decls) {
    let name = format!("%{kind}@{id}");
    *weight.entry(name.clone()).or_default() += 2 * 8usize.saturating_pow(depth.min(6));
    decls.push((id, name, kind));
}

/// Whether a function given to `map` and its relatives is compiled as the
/// body of the loop: a closure written in place that does not leave early.
fn inlined(f: &Expr) -> bool {
    matches!(&f.kind, ExprKind::Closure { body, .. } if !crate::bytecode::can_return(body))
}

/// Registers the variables a pattern introduces.
fn binds(pat: &Pattern, decls: &mut Decls) {
    match &pat.kind {
        PatKind::Bind(name) => decls.push((pat.span.start, name.clone(), VAR)),
        PatKind::Ctor { args, .. } => {
            for arg in args {
                binds(arg, decls);
            }
        }
        _ => {}
    }
}

/// How often each variable name is used, counting uses inside loops as more.
fn weigh(e: &Expr, depth: u32, weight: &mut HashMap<String, usize>, decls: &mut Decls) {
    match &e.kind {
        ExprKind::Field(base, ..) | ExprKind::Try(base) => weigh(base, depth, weight, decls),
        ExprKind::MapLit(entries) => {
            for (key, value) in entries {
                weigh(key, depth, weight, decls);
                weigh(value, depth, weight, decls);
            }
        }
        ExprKind::StructLit { fields, .. } => {
            for (_, _, value) in fields {
                weigh(value, depth, weight, decls);
            }
        }
        ExprKind::Ctor { args, .. } => {
            for arg in args {
                weigh(arg, depth, weight, decls);
            }
        }
        ExprKind::Match { scrutinee, arms } => {
            weigh(scrutinee, depth, weight, decls);
            hidden(e.span.start, SCRUT, depth, weight, decls);
            for arm in arms {
                binds(&arm.pat, decls);
                weigh(&arm.body, depth, weight, decls);
            }
        }
        ExprKind::List(items) => {
            for item in items {
                weigh(item, depth, weight, decls);
            }
        }
        ExprKind::Index(list, index) => {
            weigh(list, depth, weight, decls);
            weigh(index, depth, weight, decls);
        }
        ExprKind::MethodCall { recv, name, name_span, args } => {
            weigh(recv, depth, weight, decls);
            let id = name_span.start;
            let looping = matches!(name.as_str(), "map" | "filter" | "fold" | "any" | "all");
            if looping && !args.last().is_some_and(inlined) {
                // A function given by name or in a variable is called for each item.
                for kind in [LIST, POS, OUT, FUNC] {
                    hidden(id, kind, depth + 1, weight, decls);
                }
            }
            for arg in args {
                let ExprKind::Closure { params, body, .. } = &arg.kind else {
                    weigh(arg, depth, weight, decls);
                    continue;
                };
                if !looping || !inlined(arg) {
                    // Compiled as a function of its own.
                    continue;
                }
                // The closure becomes the body of a loop over the list.
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
            ExprKind::Int(_) | ExprKind::Float(_) | ExprKind::Bool(_) | ExprKind::Unit | ExprKind::Ident(_) => false,
            ExprKind::Unary(_, x) => self.has_call(x),
            ExprKind::Binary(_, l, r) => self.has_call(l) || self.has_call(r),
            // Reading from a struct or a list held by a variable is done in place.
            // (The same on a string is a call into the runtime.)
            ExprKind::Field(base, ..) => self.path_ty(base).is_none(),
            ExprKind::Index(list, index) => {
                !matches!(self.path_ty(list), Some(Ty::List(_))) || self.has_call(index)
            }
            // These methods exist on numbers only, and are a few instructions each.
            ExprKind::MethodCall { recv, name, args, .. } if INLINE_METHODS.contains(&name.as_str()) => {
                self.has_call(recv) || args.iter().any(|arg| self.has_call(arg))
            }
            ExprKind::MethodCall { recv, name, args, .. } => {
                !(name == "len" && args.is_empty() && matches!(self.path_ty(recv), Some(Ty::List(_))))
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

    /// One more owner for the value in `reg`.
    fn retain(&mut self, reg: u8, ty: Ty) {
        match ty {
            Ty::List(_) | Ty::Struct(_) | Ty::Fn(_) | Ty::Map(_) => self.a.inc_mem(M(reg, 0)),
            Ty::Enum(_) if self.plain_enum(ty) => {}
            Ty::Enum(_) => {
                // A variant without fields is a small number, not a block.
                let small = self.a.label();
                self.a.alu_imm(7, reg, SMALL);
                self.a.jcc(CC_B, small);
                self.a.inc_mem(M(reg, 0));
                self.a.bind(small);
            }
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

    /// Gives up the reference in rcx.
    fn drop_value(&mut self, ty: Ty) {
        match ty {
            Ty::Str => self.call_rt("str_release"),
            Ty::List(_) if !self.item_ty(ty).heap() => self.call_rt("list_release"),
            Ty::Enum(_) if self.plain_enum(ty) => {}
            Ty::List(id) | Ty::Struct(id) | Ty::Enum(id) | Ty::Fn(id) | Ty::Map(id) => {
                let label = self.glue(false, id);
                self.call_label(label);
            }
            _ => {}
        }
    }

    /// Calls `push` for a list of type `list` (rcx = the list, rdx = the item).
    fn push_call(&mut self, list: Ty) {
        match list {
            Ty::List(id) if self.item_ty(list).heap() => {
                let label = self.glue(true, id);
                self.call_label(label);
            }
            _ => self.call_rt("list_push"),
        }
    }

    /// The label of the generated code that releases a value of type `id`,
    /// or (with `push`) that pushes onto a list of type `id`.
    fn glue(&mut self, push: bool, id: u32) -> usize {
        self.uses_heap = true;
        let known = if push { self.push_labels.get(&id) } else { self.drop_labels.get(&id) };
        if let Some(label) = known {
            return *label;
        }
        let label = self.a.label();
        if push {
            self.push_labels.insert(id, label);
        } else {
            self.drop_labels.insert(id, label);
        }
        self.glue_queue.push((push, id));
        label
    }

    /// `drop(rcx = value)`: one owner less; the last one frees the block
    /// after giving up everything it holds.
    fn emit_drop(&mut self, id: u32) {
        let def = self.defs[id as usize].clone();
        let (free, done) = (self.a.label(), self.a.label());
        self.a.bind(self.drop_labels[&id]);
        if matches!(def, Def::Fn(..)) {
            // The block of a function value knows how to free itself.
            self.a.cmp_imm(M(RCX, 0), 1);
            self.a.jcc(CC_E, free);
            self.a.dec_mem(M(RCX, 0));
            self.a.b(0xC3);
            self.a.bind(free);
            self.a.buf.extend_from_slice(&[0xFF, 0x61, FN_FREE as u8]); // jmp [rcx + FN_FREE]
            return;
        }
        if matches!(def, Def::Enum(_)) {
            self.a.alu_imm(7, RCX, SMALL);
            self.a.jcc(CC_B, done);
        }
        self.a.cmp_imm(M(RCX, 0), 1);
        self.a.jcc(CC_E, free);
        self.a.dec_mem(M(RCX, 0));
        self.a.bind(done);
        self.a.b(0xC3);

        self.a.bind(free);
        self.a.push(RBX);
        self.a.push(RSI);
        self.a.alu_imm(5, RSP, 40);
        self.a.mov_rr(RBX, RCX);
        match def {
            Def::Struct(fields) => {
                for (i, (_, ty)) in fields.iter().enumerate() {
                    if ty.heap() {
                        self.a.load(RCX, M(RBX, 8 + 8 * i as i32));
                        self.drop_value(*ty);
                    }
                }
            }
            Def::Enum(variants) => {
                let end = self.a.label();
                for (tag, (_, fields)) in variants.iter().enumerate() {
                    if !fields.iter().any(|ty| ty.heap()) {
                        continue;
                    }
                    let next = self.a.label();
                    self.a.cmp_imm(M(RBX, 8), tag as i32);
                    self.a.jcc(CC_NE, next);
                    for (i, ty) in fields.iter().enumerate() {
                        if ty.heap() {
                            self.a.load(RCX, M(RBX, 16 + 8 * i as i32));
                            self.drop_value(*ty);
                        }
                    }
                    self.a.jmp(end);
                    self.a.bind(next);
                }
                self.a.bind(end);
            }
            Def::Fn(..) => {}
            Def::Map(k, v) => {
                let (top, end) = (self.a.label(), self.a.label());
                self.a.mov_imm64(RSI, 0);
                self.a.bind(top);
                self.a.alu_m(0x3B, RSI, M(RBX, LENGTH));
                self.a.jcc(CC_GE, end);
                if k.heap() {
                    self.entry(0x8B, RCX, RBX, RSI, MAP_KEY);
                    self.drop_value(k);
                }
                if v.heap() {
                    self.entry(0x8B, RCX, RBX, RSI, MAP_VALUE);
                    self.drop_value(v);
                }
                self.a.alu_imm(0, RSI, 1);
                self.a.jmp(top);
                self.a.bind(end);
            }
            Def::List(item) => {
                let (top, end) = (self.a.label(), self.a.label());
                self.a.mov_imm64(RSI, 0);
                self.a.bind(top);
                self.a.alu_m(0x3B, RSI, M(RBX, LENGTH));
                self.a.jcc(CC_GE, end);
                self.element(0x8B, RCX, RBX, RSI);
                self.drop_value(item);
                self.a.alu_imm(0, RSI, 1);
                self.a.jmp(top);
                self.a.bind(end);
            }
        }
        self.a.mov_rr(RCX, RBX);
        match self.block_size(id) {
            Some(bytes) if bytes <= POOL_MAX => {
                self.a.mov_imm64(RDX, bytes as i64);
                self.call_label(self.rt_pool_free);
            }
            _ => self.call_label(self.rt_free),
        }
        self.a.alu_imm(0, RSP, 40);
        self.a.pop(RSI);
        self.a.pop(RBX);
        self.a.b(0xC3);
    }

    /// `push(rcx = list, rdx = item) -> rax` for a list whose items are
    /// counted. When the list is shared, `list_push` copies it, and the copy
    /// then holds every item once more.
    fn emit_push(&mut self, id: u32) {
        let item = self.item_ty(Ty::List(id));
        let (top, done) = (self.a.label(), self.a.label());
        self.a.bind(self.push_labels[&id]);
        self.a.push(RBX);
        self.a.push(RSI);
        self.a.alu_imm(5, RSP, 40);
        self.a.load(RBX, M(RCX, 0));
        self.a.load(RSI, M(RCX, LENGTH));
        self.call_rt("list_push");
        self.a.alu_imm(7, RBX, 1);
        self.a.jcc(CC_E, done);
        self.a.mov_imm64(RCX, 0);
        self.a.bind(top);
        self.a.alu_rr(0x39, RCX, RSI);
        self.a.jcc(CC_GE, done);
        self.element(0x8B, RDX, RAX, RCX);
        self.retain(RDX, item);
        self.a.alu_imm(0, RCX, 1);
        self.a.jmp(top);
        self.a.bind(done);
        self.a.alu_imm(0, RSP, 40);
        self.a.pop(RSI);
        self.a.pop(RBX);
        self.a.b(0xC3);
    }

    // ---- types ----

    fn ty_expr(&mut self, t: &TypeExpr) -> R<Ty> {
        let env = self.env.clone();
        self.ty_text(&t.to_string(), &env, t.span)
    }

    /// The name in `fns` of the function `name` as used at `span`. For a
    /// generic function that is the instance for the types it is used with
    /// there, which is registered (and queued for compiling) if it is new.
    fn fn_key(&mut self, name: &str, span: Span) -> R<String> {
        let Some(f) = self.generics.get(name).cloned() else { return Ok(name.to_string()) };
        let types = self.types;
        let Some(texts) = types.call_types.get(&(span.start, span.end)) else {
            return unsupported(format!("this use of the generic function `{name}`"), span);
        };
        let env = self.env.clone();
        let mut args = Vec::new();
        for text in texts {
            args.push(self.ty_text(text, &env, span)?);
        }
        let key = format!("{name}<{}>", args.iter().map(|ty| self.ty_key(*ty)).collect::<Vec<_>>().join(", "));
        if !self.fns.contains_key(&key) {
            let inner: Vec<(String, Ty)> = f.type_params.iter().cloned().zip(args).collect();
            let mut params = Vec::new();
            for p in &f.params {
                params.push(self.ty_text(&p.ty.to_string(), &inner, p.span)?);
            }
            let ret = match &f.ret {
                Some(t) => self.ty_text(&t.to_string(), &inner, t.span)?,
                None => Ty::Unit,
            };
            let label = self.a.label();
            let contracts = !f.requires.is_empty() || !f.ensures.is_empty();
            self.fns.insert(key.clone(), FnInfo { label, params, ret, contracts });
            self.instances.push((key.clone(), f, inner));
        }
        Ok(key)
    }

    /// The type the checker found for `e`.
    fn expr_ty(&mut self, e: &Expr) -> R<Ty> {
        let types = self.types;
        let env = self.env.clone();
        match types.expr_types.get(&(e.span.start, e.span.end)) {
            Some(text) => self.ty_text(text, &env, e.span),
            None => unsupported("an expression whose type is not known", e.span),
        }
    }

    /// The type written as `text`. `env` gives the types that the type
    /// parameters in scope stand for.
    fn ty_text(&mut self, text: &str, env: &[(String, Ty)], span: Span) -> R<Ty> {
        let mut rest = text;
        let ty = self.ty_parse(&mut rest, env, span)?;
        if !rest.is_empty() {
            return unsupported(format!("the type `{text}`"), span);
        }
        Ok(ty)
    }

    fn ty_parse(&mut self, rest: &mut &str, env: &[(String, Ty)], span: Span) -> R<Ty> {
        *rest = rest.trim_start();
        if let Some(after) = rest.strip_prefix("fn(") {
            // `fn(A, B) -> R uses io`; a result that is a function type may be in parentheses.
            *rest = after;
            let mut params = Vec::new();
            loop {
                *rest = rest.trim_start();
                if let Some(after) = rest.strip_prefix(')') {
                    *rest = after;
                    break;
                }
                params.push(self.ty_parse(rest, env, span)?);
                *rest = rest.trim_start();
                match rest.strip_prefix(',') {
                    Some(after) => *rest = after,
                    None if rest.starts_with(')') => {}
                    None => return unsupported("this function type", span),
                }
            }
            let mut ret = Ty::Unit;
            if let Some(after) = rest.strip_prefix(" -> ") {
                *rest = after;
                if let Some(after) = rest.strip_prefix('(') {
                    *rest = after;
                    ret = self.ty_parse(rest, env, span)?;
                    match rest.strip_prefix(')') {
                        Some(after) => *rest = after,
                        None => return unsupported("this function type", span),
                    }
                } else {
                    ret = self.ty_parse(rest, env, span)?;
                }
            }
            // Effects do not change how a function value is represented.
            if let Some(after) = rest.strip_prefix(" uses ") {
                *rest = after;
                while let Some(effect) = EFFECTS.iter().find(|effect| rest.starts_with(**effect)) {
                    *rest = &rest[effect.len()..];
                    match rest.strip_prefix(", ") {
                        Some(after) if EFFECTS.iter().any(|effect| after.starts_with(*effect)) => *rest = after,
                        _ => break,
                    }
                }
            }
            return Ok(self.fn_ty(&params, ret));
        }
        let end = rest.find(['<', ',', '>', ')', ' ']).unwrap_or(rest.len());
        let name = rest[..end].to_string();
        *rest = &rest[end..];
        let mut args = Vec::new();
        if rest.starts_with('<') {
            loop {
                // Skips the `<` or the `,`.
                *rest = &rest[1..];
                args.push(self.ty_parse(rest, env, span)?);
                if !rest.starts_with(',') {
                    break;
                }
            }
            if !rest.starts_with('>') {
                return unsupported(format!("the type `{name}`"), span);
            }
            *rest = &rest[1..];
        }
        self.ty_apply(&name, &args, env, span)
    }

    /// The type of functions from `params` to `ret`.
    fn fn_ty(&mut self, params: &[Ty], ret: Ty) -> Ty {
        let list = params.iter().map(|ty| self.ty_key(*ty)).collect::<Vec<_>>().join(", ");
        let key = format!("fn({list}) -> {}", self.ty_key(ret));
        if let Some(ty) = self.def_ids.get(&key) {
            return *ty;
        }
        let id = self.defs.len() as u32;
        self.define(key, Ty::Fn(id), Def::Fn(params.to_vec(), ret))
    }

    fn fn_def(&self, ty: Ty) -> Option<(Vec<Ty>, Ty)> {
        match ty {
            Ty::Fn(id) => match &self.defs[id as usize] {
                Def::Fn(params, ret) => Some((params.clone(), *ret)),
                _ => None,
            },
            _ => None,
        }
    }

    fn ty_apply(&mut self, name: &str, args: &[Ty], env: &[(String, Ty)], span: Span) -> R<Ty> {
        if args.is_empty() {
            match name {
                "Int" => return Ok(Ty::Int),
                "Float" => return Ok(Ty::Float),
                "Bool" => return Ok(Ty::Bool),
                "Unit" => return Ok(Ty::Unit),
                "String" => return Ok(Ty::Str),
                "Never" => return Ok(Ty::Never),
                _ => {}
            }
            if let Some((_, ty)) = env.iter().find(|(param, _)| param == name) {
                return Ok(*ty);
            }
        }
        let key = match args {
            [] => name.to_string(),
            _ => format!("{name}<{}>", args.iter().map(|ty| self.ty_key(*ty)).collect::<Vec<_>>().join(", ")),
        };
        if let Some(ty) = self.def_ids.get(&key) {
            return Ok(*ty);
        }
        let id = self.defs.len() as u32;
        match (name, args) {
            ("List", [item]) => return Ok(self.define(key, Ty::List(id), Def::List(*item))),
            ("Map", [k, v]) => {
                if !matches!(k, Ty::Int | Ty::Str | Ty::Bool) {
                    return unsupported("a Map with keys of this type", span);
                }
                return Ok(self.define(key, Ty::Map(id), Def::Map(*k, *v)));
            }
            ("Option", [some]) => {
                let variants = vec![("None".to_string(), vec![]), ("Some".to_string(), vec![*some])];
                return Ok(self.define(key, Ty::Enum(id), Def::Enum(variants)));
            }
            ("Result", [ok, err]) => {
                let variants = vec![("Ok".to_string(), vec![*ok]), ("Err".to_string(), vec![*err])];
                return Ok(self.define(key, Ty::Enum(id), Def::Enum(variants)));
            }
            _ => {}
        }
        // A type of the program. It is registered before its parts are
        // looked at, so that it can refer to itself.
        if let Some(decl) = self.structs.get(name).copied() {
            if decl.type_params.len() == args.len() {
                let ty = self.define(key, Ty::Struct(id), Def::Struct(Vec::new()));
                let env: Vec<(String, Ty)> = decl.type_params.iter().cloned().zip(args.iter().copied()).collect();
                let mut fields = Vec::new();
                for field in &decl.fields {
                    fields.push((field.name.clone(), self.ty_text(&field.ty.to_string(), &env, field.span)?));
                }
                self.defs[id as usize] = Def::Struct(fields);
                return Ok(ty);
            }
        }
        if let Some(decl) = self.enums.get(name).copied() {
            if decl.type_params.len() == args.len() {
                let ty = self.define(key, Ty::Enum(id), Def::Enum(Vec::new()));
                let env: Vec<(String, Ty)> = decl.type_params.iter().cloned().zip(args.iter().copied()).collect();
                let mut variants = Vec::new();
                for variant in &decl.variants {
                    let mut fields = Vec::new();
                    for field in &variant.fields {
                        fields.push(self.ty_text(&field.to_string(), &env, field.span)?);
                    }
                    variants.push((variant.name.clone(), fields));
                }
                self.defs[id as usize] = Def::Enum(variants);
                return Ok(ty);
            }
        }
        unsupported(format!("the type `{name}`"), span)
    }

    fn define(&mut self, key: String, ty: Ty, def: Def) -> Ty {
        self.defs.push(def);
        self.def_keys.push(key.clone());
        self.def_ids.insert(key, ty);
        ty
    }

    fn ty_key(&self, ty: Ty) -> String {
        match ty {
            Ty::Int => "Int".to_string(),
            Ty::Float => "Float".to_string(),
            Ty::Bool => "Bool".to_string(),
            Ty::Unit => "Unit".to_string(),
            Ty::Str => "String".to_string(),
            Ty::Never => "Never".to_string(),
            Ty::List(id) | Ty::Struct(id) | Ty::Enum(id) | Ty::Fn(id) | Ty::Map(id) => self.def_keys[id as usize].clone(),
        }
    }

    /// The type of the items of a list type.
    fn item_ty(&self, list: Ty) -> Ty {
        match list {
            Ty::List(id) => match &self.defs[id as usize] {
                Def::List(item) => *item,
                _ => Ty::Never,
            },
            _ => Ty::Never,
        }
    }

    fn fields(&self, ty: Ty) -> Vec<(String, Ty)> {
        match ty {
            Ty::Struct(id) => match &self.defs[id as usize] {
                Def::Struct(fields) => fields.clone(),
                _ => Vec::new(),
            },
            _ => Vec::new(),
        }
    }

    fn variants(&self, ty: Ty) -> Vec<(String, Vec<Ty>)> {
        match ty {
            Ty::Enum(id) => match &self.defs[id as usize] {
                Def::Enum(variants) => variants.clone(),
                _ => Vec::new(),
            },
            _ => Vec::new(),
        }
    }

    /// Whether no variant of the enum has fields: its values are plain numbers.
    fn plain_enum(&self, ty: Ty) -> bool {
        match ty {
            Ty::Enum(id) => match &self.defs[id as usize] {
                Def::Enum(variants) => variants.iter().all(|(_, fields)| fields.is_empty()),
                _ => false,
            },
            _ => false,
        }
    }

    /// The size of the block of a struct or enum value. Every variant of an
    /// enum gets the size of the largest, so that the size follows from the type.
    fn block_size(&self, id: u32) -> Option<usize> {
        match &self.defs[id as usize] {
            Def::Struct(fields) => Some(8 + 8 * fields.len()),
            Def::Enum(variants) => Some(16 + 8 * variants.iter().map(|(_, fields)| fields.len()).max().unwrap_or(0)),
            Def::List(_) | Def::Fn(..) | Def::Map(..) => None,
        }
    }

    fn size_of(&self, ty: Ty) -> usize {
        match ty {
            Ty::Struct(id) | Ty::Enum(id) => self.block_size(id).unwrap_or(0),
            _ => 0,
        }
    }

    /// Allocates a counted block of `bytes` bytes with one owner, moves the
    /// parked values in `parts` (spill slot, offset in the block) into it, and
    /// leaves its address in `dst`. The slots stay taken.
    fn fill_block(&mut self, bytes: usize, parts: &[(M, i32)], dst: u8) {
        self.uses_heap = true;
        self.a.mov_imm64(RCX, bytes as i64);
        self.call_label(if bytes <= POOL_MAX { self.rt_pool_alloc } else { self.rt_alloc });
        self.a.store_imm(M(RAX, 0), 1);
        for (slot, offset) in parts {
            self.a.load(RCX, *slot);
            self.a.store(M(RAX, *offset), RCX);
        }
        self.a.mov_rr(dst, RAX);
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

        // pool_alloc(rcx = bytes, a multiple of 8 up to POOL_MAX) -> rax:
        // a freed block of that size if there is one, else the next piece
        // of the memory taken from the system.
        let (fresh, refill) = (self.a.label(), self.a.label());
        self.a.bind(self.rt_pool_alloc);
        if self.check_heap {
            self.rip_bss(&[0x48, 0xFF, 0x05], LIVE_BLOCKS); // inc qword [live blocks]
        }
        self.rip_bss(&[0x48, 0x8D, 0x15], POOL_HEADS); // lea rdx, [heads]
        self.a.buf.extend_from_slice(&[0x48, 0x8B, 0x04, 0x0A]); // mov rax, [rdx + rcx]
        self.a.alu_rr(0x85, RAX, RAX);
        self.a.jcc(CC_Z, fresh);
        self.a.buf.extend_from_slice(&[0x4C, 0x8B, 0x00]); // mov r8, [rax]
        self.a.buf.extend_from_slice(&[0x4C, 0x89, 0x04, 0x0A]); // mov [rdx + rcx], r8
        self.a.b(0xC3);
        self.a.bind(fresh);
        self.rip_bss(&[0x48, 0x8B, 0x05], POOL_NEXT); // mov rax, [next]
        self.a.buf.extend_from_slice(&[0x4C, 0x8D, 0x04, 0x08]); // lea r8, [rax + rcx]
        self.rip_bss(&[0x4C, 0x3B, 0x05], POOL_END); // cmp r8, [end]
        self.a.jcc(CC_A, refill);
        self.rip_bss(&[0x4C, 0x89, 0x05], POOL_NEXT); // mov [next], r8
        self.a.b(0xC3);
        self.a.bind(refill);
        self.a.alu_imm(5, RSP, 40);
        self.a.store(M(RSP, 48), RCX);
        self.process_heap();
        self.a.buf.extend_from_slice(&[0x31, 0xD2]); // xor edx, edx
        self.a.buf.extend_from_slice(&[0x41, 0xB8]); // mov r8d, POOL_CHUNK
        self.a.buf.extend_from_slice(&POOL_CHUNK.to_le_bytes());
        self.call_import(HEAP_ALLOC);
        self.a.alu_rr(0x85, RAX, RAX);
        self.a.jcc(CC_Z, oom);
        self.rip_bss(&[0x48, 0x89, 0x05], POOL_NEXT); // mov [next], rax
        self.a.buf.extend_from_slice(&[0x48, 0x8D, 0x90]); // lea rdx, [rax + POOL_CHUNK]
        self.a.buf.extend_from_slice(&POOL_CHUNK.to_le_bytes());
        self.rip_bss(&[0x48, 0x89, 0x15], POOL_END); // mov [end], rdx
        self.a.load(RCX, M(RSP, 48));
        self.a.alu_imm(0, RSP, 40);
        self.a.jmp(fresh);

        // pool_free(rcx = block, rdx = its size): onto the chain of its size.
        self.a.bind(self.rt_pool_free);
        if self.check_heap {
            self.rip_bss(&[0x48, 0xFF, 0x0D], LIVE_BLOCKS); // dec qword [live blocks]
        }
        self.rip_bss(&[0x4C, 0x8D, 0x05], POOL_HEADS); // lea r8, [heads]
        self.a.buf.extend_from_slice(&[0x49, 0x8B, 0x04, 0x10]); // mov rax, [r8 + rdx]
        self.a.buf.extend_from_slice(&[0x48, 0x89, 0x01]); // mov [rcx], rax
        self.a.buf.extend_from_slice(&[0x49, 0x89, 0x0C, 0x10]); // mov [r8 + rdx], rcx
        self.a.b(0xC3);

        // The operating system's functions that the runtime calls: a jump each.
        for (i, (name, _)) in OS_CALLS.iter().enumerate() {
            if self.rt_seen.contains(&format!("__os_{name}")) {
                let import = IMPORTS.iter().position(|(_, import)| import == name).unwrap_or(0);
                self.a.bind(self.os_labels[i]);
                self.a.buf.extend_from_slice(&[0xFF, 0x25]); // jmp [rip + import]
                self.iat_fixups.push((self.a.buf.len(), import));
                self.a.d32(0);
            }
        }

        // Reached with the stack as inside the routines above.
        self.a.bind(oom);
        let text = b"error: out of memory\n";
        let message = self.constant(text);
        self.lea_label(RCX, message);
        self.a.mov_imm64(RDX, text.len() as i64);
        self.call_label(self.rt_fail);
    }

    /// More of the runtime: errors that are put together when they happen.
    fn error_runtime(&mut self) {
        // number(rcx = value, r8 = record): the record holds two lengths (4
        // bytes each) and then two pieces of text; the value is printed between them.
        self.a.bind(self.rt_number);
        self.a.alu_imm(5, RSP, 40);
        self.a.mov_rr(RBX, RCX);
        self.a.mov_rr(RDI, R8);
        self.a.buf.extend_from_slice(&[0x48, 0x8D, 0x4F, 8]); // lea rcx, [rdi + 8]
        self.a.buf.extend_from_slice(&[0x8B, 0x17]); // mov edx, [rdi]
        self.a.mov_imm64(R8, STDERR);
        self.call_label(self.rt_write);
        self.a.mov_rr(RCX, RBX);
        self.a.mov_imm64(RDX, STDERR);
        self.call_label(self.rt_print_int);
        self.a.buf.extend_from_slice(&[0x8B, 0x07]); // mov eax, [rdi]
        self.a.buf.extend_from_slice(&[0x48, 0x8D, 0x4C, 0x07, 8]); // lea rcx, [rdi + rax + 8]
        self.a.buf.extend_from_slice(&[0x8B, 0x57, 4]); // mov edx, [rdi + 4]
        self.a.mov_imm64(R8, STDERR);
        self.call_label(self.rt_write);
        self.a.buf.extend_from_slice(&[0xB9, 1, 0, 0, 0]); // mov ecx, 1
        self.call_import(EXIT_PROCESS);

        // contract(rcx = text, rdx = length, r8 = the caller's place: a
        // length (4 bytes) and text): writes both and exits with 1.
        self.a.bind(self.rt_contract);
        self.a.alu_imm(5, RSP, 40);
        self.a.mov_rr(RBX, R8);
        self.a.mov_imm64(R8, STDERR);
        self.call_label(self.rt_write);
        self.a.buf.extend_from_slice(&[0x48, 0x8D, 0x4B, 4]); // lea rcx, [rbx + 4]
        self.a.buf.extend_from_slice(&[0x8B, 0x13]); // mov edx, [rbx]
        self.a.mov_imm64(R8, STDERR);
        self.call_label(self.rt_write);
        self.a.buf.extend_from_slice(&[0xB9, 1, 0, 0, 0]); // mov ecx, 1
        self.call_import(EXIT_PROCESS);
    }

    /// What a caller tells a function with contracts about itself: the part
    /// of the error text that shows the place of the call.
    fn site(&mut self, span: Span) -> usize {
        self.uses_errors = true;
        let text = self.error_text(&Diagnostic::new("E0502", "", span));
        let place = text.split_once('\n').map(|(_, rest)| rest).unwrap_or("");
        let mut blob = (place.len() as u32).to_le_bytes().to_vec();
        blob.extend(place.as_bytes());
        self.constant(&blob)
    }

    /// Reserves code that reports `d`, whose message has the marks `\u{1}`,
    /// `\u{2}`, `\u{3}` where the `values` go, in that order.
    fn report(&mut self, d: Diagnostic, values: Vec<Part>) -> usize {
        let text = self.error_text(&d);
        let mut rest = text.as_str();
        let mut parts = Vec::new();
        let mut values = values.into_iter();
        for mark in ['\u{1}', '\u{2}', '\u{3}'] {
            if let Some((before, after)) = rest.split_once(mark) {
                parts.push(Part::Text(self.constant(before.as_bytes()), before.len()));
                parts.extend(values.next());
                rest = after;
            }
        }
        parts.push(Part::Text(self.constant(rest.as_bytes()), rest.len()));
        let label = self.a.label();
        self.stubs.push(Stub::Report { label, parts });
        label
    }

    /// Reserves code that reports an error whose message contains the value
    /// of `value` where `d`'s message has the mark `\u{1}`.
    fn number_stub(&mut self, value: u8, d: Diagnostic) -> usize {
        self.uses_errors = true;
        let text = self.error_text(&d);
        let (before, after) = text.split_once('\u{1}').unwrap_or((&text, ""));
        let mut record = Vec::new();
        record.extend((before.len() as u32).to_le_bytes());
        record.extend((after.len() as u32).to_le_bytes());
        record.extend(before.as_bytes());
        record.extend(after.as_bytes());
        let record = self.constant(&record);
        let label = self.a.label();
        self.stubs.push(Stub::Number { label, value, record });
        label
    }

    /// The text an executable made by `yui build` prints for a runtime
    /// error, whichever way it was built.
    fn error_text(&self, d: &Diagnostic) -> String {
        format!("{}\n", render_human(d, self.sources))
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
                Stub::Push { label, back, variable, value, list } => {
                    self.a.bind(label);
                    self.a.mov_rr(RDX, value);
                    self.load_loc(RCX, variable);
                    self.push_call(list);
                    self.store_loc(variable, RAX);
                    self.a.jmp(back);
                }
                Stub::Unshare { label, back, variable, root, fields, a, b, saves } => {
                    self.a.bind(label);
                    // The copies are made by calls, which overwrite r8-r11.
                    let parked: Vec<(u8, M)> = [a, b]
                        .iter()
                        .zip(saves)
                        .filter_map(|(o, slot)| match o {
                            Opnd::Reg(r) if TEMPS.contains(r) => Some((*r, slot)),
                            _ => None,
                        })
                        .collect();
                    for (r, slot) in &parked {
                        self.a.store(*slot, *r);
                    }
                    // The variable's own value: the other owners keep it as it was.
                    let unique = self.a.label();
                    self.load_loc(RAX, variable);
                    self.a.cmp_imm(M(RAX, 0), 1);
                    self.a.jcc(CC_E, unique);
                    self.a.mov_rr(RCX, RAX);
                    let clone = self.clone_label(root);
                    self.call_label(clone);
                    self.load_loc(RCX, variable);
                    self.a.dec_mem(M(RCX, 0));
                    self.store_loc(variable, RAX);
                    self.a.bind(unique);
                    // Then each field on the way, in the value reached so far (rax).
                    for (offset, ty) in fields {
                        let (same, next) = (self.a.label(), self.a.label());
                        self.a.store(saves[2], RAX);
                        self.a.load(RCX, M(RAX, offset));
                        self.a.cmp_imm(M(RCX, 0), 1);
                        self.a.jcc(CC_E, same);
                        let clone = self.clone_label(ty);
                        self.call_label(clone);
                        self.a.load(RDX, saves[2]);
                        self.a.load(RCX, M(RDX, offset));
                        self.a.dec_mem(M(RCX, 0));
                        self.a.store(M(RDX, offset), RAX);
                        self.a.jmp(next);
                        self.a.bind(same);
                        self.a.mov_rr(RAX, RCX);
                        self.a.bind(next);
                    }
                    for (r, slot) in &parked {
                        self.a.load(*r, *slot);
                    }
                    self.a.jmp(back);
                }
                Stub::Missing { label, key, ty, before, after } => {
                    self.a.bind(label);
                    self.lea_label(RCX, before.0);
                    self.a.mov_imm64(RDX, before.1 as i64);
                    self.a.mov_imm64(R8, STDERR);
                    self.call_label(self.rt_write);
                    self.a.mov_imm64(RCX, 32);
                    self.call_rt("str_new");
                    self.a.mov_rr(RCX, RAX);
                    self.a.load(RDX, key);
                    self.repr_call(ty);
                    self.a.lea(RCX, M(RAX, ELEMENTS));
                    self.a.load(RDX, M(RAX, LENGTH));
                    self.a.mov_imm64(R8, STDERR);
                    self.call_label(self.rt_write);
                    self.lea_label(RCX, after.0);
                    self.a.mov_imm64(RDX, after.1 as i64);
                    self.call_label(self.rt_fail);
                }
                Stub::Report { label, parts } => {
                    self.a.bind(label);
                    let last = parts.len() - 1;
                    for (i, part) in parts.into_iter().enumerate() {
                        match part {
                            Part::Text(text, len) if i == last => {
                                self.lea_label(RCX, text);
                                self.a.mov_imm64(RDX, len as i64);
                                self.call_label(self.rt_fail);
                            }
                            Part::Text(text, len) => {
                                self.lea_label(RCX, text);
                                self.a.mov_imm64(RDX, len as i64);
                                self.a.mov_imm64(R8, STDERR);
                                self.call_label(self.rt_write);
                            }
                            Part::Value(at, ty) => {
                                self.a.mov_imm64(RCX, 32);
                                self.call_rt("str_new");
                                self.a.mov_rr(RCX, RAX);
                                self.a.load(RDX, at);
                                self.repr_call(ty);
                                self.a.lea(RCX, M(RAX, ELEMENTS));
                                self.a.load(RDX, M(RAX, LENGTH));
                                self.a.mov_imm64(R8, STDERR);
                                self.call_label(self.rt_write);
                            }
                            Part::Raw(at) => {
                                self.a.load(RAX, at);
                                self.a.lea(RCX, M(RAX, ELEMENTS));
                                self.a.load(RDX, M(RAX, LENGTH));
                                self.a.mov_imm64(R8, STDERR);
                                self.call_label(self.rt_write);
                            }
                        }
                    }
                }
                Stub::Number { label, value, record } => {
                    self.a.bind(label);
                    self.mov(RCX, value);
                    self.lea_label(R8, record);
                    self.call_label(self.rt_number);
                }
                Stub::Contract { label, header, len, site, result } => {
                    self.a.bind(label);
                    match result {
                        None => {
                            self.lea_label(RCX, header);
                            self.a.mov_imm64(RDX, len as i64);
                        }
                        Some((ty, at)) => {
                            // The first line so far, then the result as text, then its end.
                            self.lea_label(RCX, header);
                            self.a.mov_imm64(RDX, len as i64);
                            self.a.mov_imm64(R8, STDERR);
                            self.call_label(self.rt_write);
                            if ty == Ty::Unit {
                                self.write(b"()", STDERR);
                            } else {
                                self.a.mov_imm64(RCX, 32);
                                self.call_rt("str_new");
                                self.a.mov_rr(RCX, RAX);
                                self.load_loc(RDX, at);
                                self.repr_call(ty);
                                self.a.lea(RCX, M(RAX, ELEMENTS));
                                self.a.load(RDX, M(RAX, LENGTH));
                                self.a.mov_imm64(R8, STDERR);
                                self.call_label(self.rt_write);
                            }
                            let end = self.constant(b")\n");
                            self.lea_label(RCX, end);
                            self.a.mov_imm64(RDX, 2);
                        }
                    }
                    self.load_loc(R8, site);
                    self.call_label(self.rt_contract);
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
        let info = &self.fns[key];
        let (label, ret) = (info.label, info.ret);
        let params: Vec<(String, usize, Ty)> =
            f.params.iter().zip(&info.params).map(|(p, ty)| (p.name.clone(), p.span.start, *ty)).collect();
        self.emit_fn(label, &params, &[], ret, Body::Block(&f.body), f.name_span, Some(f))
    }

    /// Compiles the body of a closure, and the code that frees its block.
    fn closure(&mut self, c: Closure) -> R<()> {
        self.emit_fn(c.code, &c.params, &c.captures, c.ret, Body::Expr(&c.body), c.body.span, None)?;
        self.a.bind(c.free);
        let counted: Vec<(usize, Ty)> =
            c.captures.iter().enumerate().filter(|(_, (_, ty))| ty.heap()).map(|(i, (_, ty))| (i, *ty)).collect();
        if !counted.is_empty() {
            // Gives up what was captured first.
            self.a.push(RBX);
            self.a.alu_imm(5, RSP, 32);
            self.a.mov_rr(RBX, RCX);
            for (i, ty) in counted {
                self.a.load(RCX, M(RBX, FN_CAPTURES + 8 * i as i32));
                self.drop_value(ty);
            }
            self.a.mov_rr(RCX, RBX);
            self.a.alu_imm(0, RSP, 32);
            self.a.pop(RBX);
        }
        if c.bytes <= POOL_MAX {
            self.a.mov_imm64(RDX, c.bytes as i64);
            self.a.jmp(self.rt_pool_free);
        } else {
            self.a.jmp(self.rt_free);
        }
        Ok(())
    }

    /// `rcx = the entry of the table of remembered results` (at `table` in the
    /// writable memory) for the arguments in `args`. Uses rax and rdx.
    fn memo_entry(&mut self, args: &[Loc], table: usize) {
        self.load_loc(RAX, args[0]);
        for loc in &args[1..] {
            self.a.imul_imm(RAX, 1_000_003);
            match loc {
                Loc::Reg(r) => self.a.alu_rr(0x01, RAX, *r),
                Loc::Stack(off) => self.a.alu_m(0x03, RAX, M(RSP, *off)),
            }
        }
        self.a.alu_imm(4, RAX, MEMO_ENTRIES as i32 - 1); // and
        self.a.imul_imm(RAX, (args.len() as i32 + 2) * 8);
        self.rip_bss(&[0x48, 0x8D, 0x0D], table); // lea rcx, [table]
        self.a.alu_rr(0x01, RCX, RAX);
    }

    /// Compiles one function. With `captures`, it is the code of a closure:
    /// the closure's block arrives in rcx, before the parameters, and the
    /// captured values are read out of it. `decl` gives the contracts.
    #[allow(clippy::too_many_arguments)]
    fn emit_fn(
        &mut self,
        label: usize,
        params: &[(String, usize, Ty)],
        captures: &[(String, Ty)],
        ret: Ty,
        body: Body,
        span: Span,
        decl: Option<&FnDecl>,
    ) -> R<()> {
        let (requires, ensures) = match decl {
            Some(f) => (&f.requires[..], &f.ensures[..]),
            None => (&[][..], &[][..]),
        };
        let contracts = !requires.is_empty() || !ensures.is_empty();
        // Decide where every variable lives: the most used ones in registers.
        let mut weight = HashMap::new();
        let mut decls: Decls = params.iter().map(|(name, at, _)| (*at, name.clone(), VAR)).collect();
        decls.extend(captures.iter().enumerate().map(|(i, (name, _))| (i, name.clone(), CAPTURE)));
        match &body {
            Body::Block(block) => weigh_block(block, 0, &mut weight, &mut decls),
            Body::Expr(e) => weigh(e, 0, &mut weight, &mut decls),
        }
        for clause in requires.iter().chain(ensures) {
            weigh(clause, 0, &mut weight, &mut decls);
        }
        if contracts {
            decls.push((0, "%site".to_string(), SITE));
            decls.push((0, "result".to_string(), RESULT));
        }
        // Arguments beyond the fourth of a call are passed in memory, just
        // above the 32 bytes every call may use; the variables come after that.
        let mut most = 0;
        let mut count = |e: &Expr| {
            if let ExprKind::Call { args, local, .. } = &e.kind {
                most = most.max(args.len() + *local as usize);
            }
            false
        };
        match &body {
            Body::Block(block) => any_block(block, &mut count),
            Body::Expr(e) => any_expr(e, &mut count),
        };
        for clause in requires.iter().chain(ensures) {
            any_expr(clause, &mut count);
        }
        let outgoing = 32 + 8 * (most.max(4) - 4) as i32;
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
                    Loc::Stack(outgoing + 8 * (stack_slots - 1))
                }
            };
            let (id, _, kind) = &decls[i];
            self.decl_loc.insert((*id, *kind), loc);
        }
        self.spill_base = outgoing + 8 * stack_slots;
        self.spill_depth = 0;
        self.spill_max = 0;
        self.free = TEMPS.to_vec();
        self.loops.clear();
        self.pending.clear();
        self.epilogue = self.a.label();
        self.ret = ret;

        self.a.bind(label);
        for reg in &used {
            self.a.push(*reg);
        }
        self.a.alu_imm(5, RSP, 0);
        let frame_at = [self.a.buf.len() - 4, 0];
        let mut scope = HashMap::new();
        // In the code of a function value, the block takes the first place.
        let is_value = !captures.is_empty() || matches!(body, Body::Expr(_));
        let site = self.decl_loc.get(&(0, SITE)).copied();
        if let Some(site) = site {
            // The caller left the description of its place in rax.
            self.store_loc(site, RAX);
        }
        // Parameters after the fourth are in the caller's frame; how far
        // that is from here is known once this function's frame is sized.
        let mut incoming = Vec::new();
        for (i, (name, at, ty)) in params.iter().enumerate() {
            let loc = self.decl_loc[&(*at, VAR)];
            let k = i + is_value as usize;
            if k < 4 {
                self.store_loc(loc, ARG_REGS[k]);
            } else {
                self.a.load(RAX, M(RSP, 0));
                incoming.push((self.a.buf.len() - 4, 8 * used.len() as i32 + 40 + 8 * (k as i32 - 4)));
                self.store_loc(loc, RAX);
            }
            scope.insert(name.clone(), Local { ty: *ty, loc, borrowed: false });
        }
        for (i, (name, ty)) in captures.iter().enumerate() {
            // Borrowed from the block, which whoever called keeps alive.
            let loc = self.decl_loc[&(i, CAPTURE)];
            self.a.load(RAX, M(RCX, FN_CAPTURES + 8 * i as i32));
            self.store_loc(loc, RAX);
            scope.insert(name.clone(), Local { ty: *ty, loc, borrowed: true });
        }
        self.scopes = vec![scope];
        let name = decl.map(|f| f.name.rsplit('.').next().unwrap_or(&f.name).to_string()).unwrap_or_default();
        // A function without effects gives the same result for the same
        // arguments, so results can be remembered and reused. That pays off
        // when the function calls itself more than once per call, which is
        // where the same arguments come up again and again. (Rust cannot do
        // this by itself: it does not know that a function has no effects.)
        let ints: Vec<Loc> = params.iter().map(|(_, at, _)| self.decl_loc[&(*at, VAR)]).collect();
        let memo = decl.filter(|f| {
            let mut calls = 0;
            any_block(&f.body, &mut |e| {
                if matches!(&e.kind, ExprKind::Call { name, local: false, .. } if *name == f.name) {
                    calls += 1;
                }
                false
            });
            !self.in_runtime
                && f.effects.is_empty()
                && ret == Ty::Int
                && (1..=3).contains(&params.len())
                && params.iter().all(|(_, _, ty)| *ty == Ty::Int)
                && calls >= 2
        });
        let quick_exit = self.a.label();
        let table = memo.map(|_| {
            // Each entry: whether it is filled, the arguments, the result.
            let at = self.bss_used;
            self.bss_used += MEMO_ENTRIES * 8 * (params.len() + 2);
            at
        });
        if let Some(table) = table {
            let miss = self.a.label();
            self.memo_entry(&ints, table);
            self.a.cmp_imm(M(RCX, 0), 1);
            self.a.jcc(CC_NE, miss);
            for (i, loc) in ints.iter().enumerate() {
                self.a.load(RAX, M(RCX, 8 + 8 * i as i32));
                match loc {
                    Loc::Reg(r) => self.a.alu_rr(0x39, RAX, *r),
                    Loc::Stack(off) => self.a.alu_m(0x3B, RAX, M(RSP, *off)),
                }
                self.a.jcc(CC_NE, miss);
            }
            self.a.load(RAX, M(RCX, 8 + 8 * ints.len() as i32));
            self.a.jmp(quick_exit);
            self.a.bind(miss);
        }
        for clause in requires {
            let Some(site) = site else { break };
            let text = &self.sources.text[clause.span.start..clause.span.end];
            let header = format!("error[E0502]: precondition of `{name}` violated: `{text}`\n");
            let (label, len) = (self.a.label(), header.len());
            let header = self.constant(header.as_bytes());
            self.stubs.push(Stub::Contract { label, header, len, site, result: None });
            self.cond(clause, label)?;
        }

        let t = self.take_temp(span)?;
        let ty = match body {
            Body::Block(block) => self.block(block, t)?,
            Body::Expr(e) => self.eval(e, t)?,
        };
        if ret != Ty::Unit && ty != Ty::Never {
            self.a.mov_rr(RAX, t);
        }
        self.free.push(t);

        self.a.bind(self.epilogue);
        if let (Some(site), false) = (site, ensures.is_empty()) {
            // `result` in an `ensures` clause is what the function returns.
            let at = self.decl_loc[&(0, RESULT)];
            if ret != Ty::Unit {
                self.store_loc(at, RAX);
            }
            self.scopes.push(HashMap::from([("result".to_string(), Local { ty: ret, loc: at, borrowed: true })]));
            for clause in ensures {
                let text = &self.sources.text[clause.span.start..clause.span.end];
                let header = format!("error[E0502]: postcondition of `{name}` violated: `{text}` (result: ");
                let (label, len) = (self.a.label(), header.len());
                let header = self.constant(header.as_bytes());
                self.stubs.push(Stub::Contract { label, header, len, site, result: Some((ret, at)) });
                self.cond(clause, label)?;
            }
            self.scopes.pop();
            if ret != Ty::Unit {
                self.load_loc(RAX, at);
            }
        }
        // The parameters are the function's own references.
        self.release_scopes(0, (ret != Ty::Unit).then_some(RAX));
        if let Some(table) = table {
            // Parameters are never assigned to, so they still hold the arguments.
            self.a.mov_rr(R8, RAX);
            self.memo_entry(&ints, table);
            self.a.store_imm(M(RCX, 0), 1);
            for (i, loc) in ints.iter().enumerate() {
                self.load_loc(RAX, *loc);
                self.a.store(M(RCX, 8 + 8 * i as i32), RAX);
            }
            self.a.store(M(RCX, 8 + 8 * ints.len() as i32), R8);
            self.a.mov_rr(RAX, R8);
        }
        self.a.bind(quick_exit);
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
        for (at, above) in incoming {
            self.a.buf[at..at + 4].copy_from_slice(&(frame + above).to_le_bytes());
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
            if old.ty.heap() && !old.borrowed {
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
            self.scopes[from..].iter().flat_map(|s| s.values()).filter(|l| l.ty.heap() && !l.borrowed).copied().collect();
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
            self.drop_value(local.ty);
        }
        if let (Some(reg), Some(slot)) = (keep, slot) {
            self.a.load(reg, slot);
            self.spill_depth -= 1;
        }
    }

    /// Leaving early: gives up the values held by expressions in progress
    /// (from `held` on) and by the variables of the scopes from `from` on.
    fn unwind(&mut self, from: usize, held: usize, keep: Option<u8>) {
        let pending: Vec<(M, Ty)> = self.pending[held..].to_vec();
        if !pending.is_empty() {
            let slot = keep.map(|reg| {
                let slot = self.push_spill();
                self.a.store(slot, reg);
                slot
            });
            for (at, ty) in pending {
                self.a.load(RCX, at);
                self.drop_value(ty);
            }
            if let (Some(reg), Some(slot)) = (keep, slot) {
                self.a.load(reg, slot);
                self.spill_depth -= 1;
            }
        }
        self.release_scopes(from, keep);
    }

    /// Notes that the spill slot `at` owns a value of type `ty` for now.
    /// Undone by cutting `pending` back to its length before.
    fn hold(&mut self, at: M, ty: Ty) {
        if ty.heap() {
            self.pending.push((at, ty));
        }
    }

    /// Gives up the reference in rcx without losing the value in `keep`.
    fn release_keeping(&mut self, keep: u8, ty: Ty) {
        let slot = self.push_spill();
        self.a.store(slot, keep);
        self.drop_value(ty);
        self.a.load(keep, slot);
        self.spill_depth -= 1;
    }

    /// A value that is dropped must not be counted: nothing would release it.
    /// (The type checker already rejects unused values.)
    fn discard(&self, ty: Ty, span: Span) -> R<()> {
        if ty.heap() {
            return unsupported("a value that is not used", span);
        }
        Ok(())
    }

    /// The variable `e` names, if what it holds is counted.
    fn heap_var(&self, e: &Expr) -> Option<Local> {
        let ExprKind::Ident(name) = &e.kind else { return None };
        let local = self.scopes.iter().rev().find_map(|s| s.get(name.as_str()))?;
        local.ty.heap().then_some(*local)
    }

    /// The type of `e` if it can be read where it is: a variable holding a
    /// counted value, or a counted field (of a field ...) of one. The
    /// variable keeps everything on the way alive.
    fn path_ty(&self, e: &Expr) -> Option<Ty> {
        match &e.kind {
            ExprKind::Ident(_) => self.heap_var(e).map(|local| local.ty),
            ExprKind::Field(base, name, _) => {
                let ty = self.path_ty(base)?;
                self.fields(ty).into_iter().find(|(field, _)| field == name).map(|(_, ty)| ty).filter(|ty| ty.heap())
            }
            _ => None,
        }
    }

    /// Loads into `reg` the value `path_ty` describes, without counting it.
    fn load_path(&mut self, e: &Expr, reg: u8) {
        match &e.kind {
            ExprKind::Field(base, name, _) => {
                self.load_path(base, reg);
                let ty = self.path_ty(base).unwrap_or(Ty::Never);
                let at = self.fields(ty).iter().position(|(field, _)| field == name).unwrap_or(0);
                self.a.load(reg, M(reg, 8 + 8 * at as i32));
            }
            _ => {
                if let Some(local) = self.heap_var(e) {
                    self.load_loc(reg, local.loc);
                }
            }
        }
    }

    /// The register holding the list `e` (see `path_ty`): the variable's own, or rcx.
    fn path_reg(&mut self, e: &Expr) -> u8 {
        match self.heap_var(e) {
            Some(local) => self.reg(local.loc, RCX),
            None => {
                self.load_path(e, RCX);
                RCX
            }
        }
    }

    /// Puts the value of `e` in a new spill slot, using `dst` on the way.
    /// A list or string held by a variable is only borrowed (the variable
    /// keeps it alive); otherwise the slot owns a reference. Returns the
    /// slot, whether it owns, and the type.
    fn park(&mut self, e: &Expr, dst: u8) -> R<(M, bool, Ty)> {
        let (owned, ty) = match self.path_ty(e) {
            Some(ty) => {
                self.load_path(e, dst);
                (false, ty)
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
            let held = self.pending.len();
            if left.1 {
                self.hold(left.0, Ty::Str);
            }
            let (slot, owned, ty) = self.park(right, dst)?;
            self.pending.truncate(held);
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

    /// What `e` evaluates to, where that is plain to see. Only used to guess sizes.
    fn quick_ty(&self, e: &Expr) -> Option<Ty> {
        match &e.kind {
            ExprKind::Int(_) => Some(Ty::Int),
            ExprKind::Float(_) => Some(Ty::Float),
            ExprKind::Bool(_) => Some(Ty::Bool),
            ExprKind::Ident(name) => self.scopes.iter().rev().find_map(|s| s.get(name.as_str())).map(|l| l.ty),
            ExprKind::Binary(op, ..) => match op {
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => Some(Ty::Int),
                _ => Some(Ty::Bool),
            },
            ExprKind::MethodCall { name, .. } if name == "len" => Some(Ty::Int),
            _ => None,
        }
    }

    /// The type of the receiver of a method call.
    fn recv_ty(&mut self, e: &Expr) -> R<Ty> {
        match &e.kind {
            ExprKind::Ident(name) => Ok(self.lookup(name, e.span)?.ty),
            ExprKind::Int(_) => Ok(Ty::Int),
            ExprKind::Float(_) => Ok(Ty::Float),
            ExprKind::Bool(_) => Ok(Ty::Bool),
            _ => self.expr_ty(e),
        }
    }

    /// The variable `e` names, if it holds a list.
    fn list_var(&self, e: &Expr) -> Option<Local> {
        self.heap_var(e).filter(|local| matches!(local.ty, Ty::List(_)))
    }

    /// Evaluates `e`, which must be a list, into `dst`; `dst` then owns a
    /// reference. Returns the type of the list.
    fn owned_list(&mut self, e: &Expr, dst: u8) -> R<Ty> {
        match self.eval(e, dst)? {
            ty @ Ty::List(_) => Ok(ty),
            _ => unsupported("this operation on anything but a list", e.span),
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
                    // `let w = v` in the body of a function, with `v` not used
                    // after it: `w` takes over what `v` holds. (The statements
                    // of the body run once, in order, so "after" is what follows.)
                    if let Stmt::Let { value, .. } = other {
                        if let ExprKind::Ident(source) = &value.kind {
                            let later = &b.stmts[i + 1..];
                            if self.scopes.len() == 2 && !later.iter().any(|s| any_stmt(s, &mut |e| refers(e, source))) {
                                self.moving = Some((source.clone(), true));
                            }
                        }
                    }
                    self.stmt(other)?;
                    self.moving = None;
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
                if matches!(ty, Ty::Unit | Ty::Never) {
                    return unsupported("a variable that holds no value", *name_span);
                }
                // `_` cannot be read, but what it holds is still released with the scope.
                if name != "_" || ty.heap() {
                    self.declare(name, Local { ty, loc, borrowed: false });
                }
            }
            Stmt::Assign { target, value } => {
                let ExprKind::Ident(name) = &target.kind else {
                    return self.assign_place(target, value);
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
                self.loops.push((exit, top, self.scopes.len(), self.pending.len()));
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
                // A loop that only pushes onto a list adds exactly one item
                // per round: make room for all of them at once.
                if let [Stmt::Assign { target, value }] = &body.stmts[..] {
                    if let (ExprKind::Ident(list), ExprKind::MethodCall { recv, name, args, .. }) = (&target.kind, &value.kind) {
                        let itself = matches!(&recv.kind, ExprKind::Ident(n) if n == list);
                        let local = self.list_var(target).filter(|local| !local.borrowed);
                        if let (true, "push", 1, Some(local)) = (itself, name.as_str(), args.len(), local) {
                            self.load_loc(RDX, limit);
                            match counter {
                                Loc::Reg(r) => self.a.alu_rr(0x29, RDX, r),
                                Loc::Stack(off) => self.a.alu_m(0x2B, RDX, M(RSP, off)),
                            }
                            self.load_loc(RCX, local.loc);
                            self.call_rt("list_reserve");
                            self.store_loc(local.loc, RAX);
                        }
                    }
                }
                let (top, next, exit) = (self.a.label(), self.a.label(), self.a.label());
                self.a.bind(top);
                self.load_loc(RAX, counter);
                match limit {
                    Loc::Reg(r) => self.a.alu_rr(0x39, RAX, r),
                    Loc::Stack(off) => self.a.alu_m(0x3B, RAX, M(RSP, off)),
                }
                self.a.jcc(CC_GE, exit);
                self.scopes.push(HashMap::from([(var.clone(), Local { ty: Ty::Int, loc: counter, borrowed: false })]));
                self.loops.push((exit, next, self.scopes.len(), self.pending.len()));
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
                let list_ty = self.eval_into(iter, list, true)?;
                if !matches!(list_ty, Ty::List(_)) {
                    return unsupported("`for` over anything but a list", iter.span);
                }
                self.set_loc(pos, 0);
                let (top, next, exit) = (self.a.label(), self.a.label(), self.a.label());
                // The loop variable is borrowed from the list, which the loop keeps alive.
                let item_ty = self.item_ty(list_ty);
                self.scopes.push(HashMap::from([
                    ("%for".to_string(), Local { ty: list_ty, loc: list, borrowed: false }),
                    (var.clone(), Local { ty: item_ty, loc: item, borrowed: true }),
                ]));
                self.a.bind(top);
                self.next_item(list, pos, item, exit);
                self.loops.push((exit, next, self.scopes.len(), self.pending.len()));
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
                self.drop_value(list_ty);
            }
            Stmt::Assert { cond, span } => {
                let text = self.sources.text[cond.span.start..cond.span.end].to_string();
                match &cond.kind {
                    // A comparison that fails shows both sides.
                    ExprKind::Binary(op, l, r) if op.is_comparison() => self.assert_cmp(*op, l, r, &text, *span)?,
                    _ => {
                        let stub = self.fail_stub(Diagnostic::new("E0501", format!("assertion failed: `{text}`"), *span));
                        self.cond(cond, stub)?;
                    }
                }
            }
            Stmt::Expr(e) => {
                let t = self.take_temp(e.span)?;
                let ty = self.eval(e, t)?;
                self.discard(ty, e.span)?;
                self.free.push(t);
            }
        }
        Ok(())
    }

    /// The label of the generated code that copies a value of type `ty`.
    fn clone_label(&mut self, ty: Ty) -> usize {
        let (Ty::List(id) | Ty::Struct(id) | Ty::Map(id)) = ty else { return self.rt_fail };
        self.uses_heap = true;
        if let Some(label) = self.clone_labels.get(&id) {
            return *label;
        }
        let label = self.a.label();
        self.clone_labels.insert(id, label);
        self.clone_queue.push(id);
        label
    }

    /// `clone(rcx = value) -> rax`: a new block with one owner and the same
    /// contents; what it holds is held once more.
    fn emit_clone(&mut self, id: u32) {
        let def = self.defs[id as usize].clone();
        self.a.bind(self.clone_labels[&id]);
        self.a.push(RBX);
        self.a.push(RSI);
        self.a.alu_imm(5, RSP, 40);
        self.a.mov_rr(RBX, RCX);
        match def {
            Def::Struct(fields) => {
                let bytes = 8 + 8 * fields.len();
                self.a.mov_imm64(RCX, bytes as i64);
                self.call_label(if bytes <= POOL_MAX { self.rt_pool_alloc } else { self.rt_alloc });
                self.a.store_imm(M(RAX, 0), 1);
                for (i, (_, ty)) in fields.iter().enumerate() {
                    let at = 8 + 8 * i as i32;
                    self.a.load(RDX, M(RBX, at));
                    self.retain(RDX, *ty);
                    self.a.store(M(RAX, at), RDX);
                }
            }
            Def::List(item) => {
                let (top, done) = (self.a.label(), self.a.label());
                self.a.load(RCX, M(RBX, LENGTH));
                self.call_rt("list_new");
                self.a.load(RSI, M(RBX, LENGTH));
                self.a.store(M(RAX, LENGTH), RSI);
                self.a.mov_imm64(RCX, 0);
                self.a.bind(top);
                self.a.alu_rr(0x39, RCX, RSI);
                self.a.jcc(CC_GE, done);
                self.element(0x8B, RDX, RBX, RCX);
                self.retain(RDX, item);
                self.element(0x89, RDX, RAX, RCX);
                self.a.alu_imm(0, RCX, 1);
                self.a.jmp(top);
                self.a.bind(done);
            }
            Def::Map(k, v) => {
                let (top, done) = (self.a.label(), self.a.label());
                self.a.load(RCX, M(RBX, LENGTH));
                self.call_rt("map_new");
                self.a.load(RSI, M(RBX, LENGTH));
                self.a.store(M(RAX, LENGTH), RSI);
                // The new map is kept in r9: reaching an entry uses rax.
                self.a.mov_rr(R9, RAX);
                self.a.mov_imm64(RCX, 0);
                self.a.bind(top);
                self.a.alu_rr(0x39, RCX, RSI);
                self.a.jcc(CC_GE, done);
                for (at, ty) in [(MAP_KEY, k), (MAP_VALUE, v)] {
                    self.entry(0x8B, R8, RBX, RCX, at);
                    self.retain(R8, ty);
                    self.entry(0x89, R8, R9, RCX, at);
                }
                self.a.alu_imm(0, RCX, 1);
                self.a.jmp(top);
                self.a.bind(done);
                self.a.mov_rr(RAX, R9);
            }
            _ => {}
        }
        self.a.alu_imm(0, RSP, 40);
        self.a.pop(RSI);
        self.a.pop(RBX);
        self.a.b(0xC3);
    }

    /// `place = value` where the place is an item of a list or a field of a
    /// struct, possibly nested (`rows[i][j]`, `p.items[i]`), in a variable.
    /// Values are not shared with whoever else holds them: every list or
    /// struct on the way that has another owner is copied first.
    fn assign_place(&mut self, target: &Expr, value: &Expr) -> R<()> {
        enum Step<'e> {
            Field(&'e str, Span),
            Index(&'e Expr, Span),
        }
        let mut steps = Vec::new();
        let mut root = target;
        loop {
            match &root.kind {
                ExprKind::Field(base, name, name_span) => {
                    steps.push(Step::Field(name, *name_span));
                    root = base;
                }
                ExprKind::Index(base, index) => {
                    steps.push(Step::Index(index, root.span));
                    root = base;
                }
                _ => break,
            }
        }
        steps.reverse();
        let ExprKind::Ident(name) = &root.kind else {
            return unsupported("assigning to this", target.span);
        };
        let var = self.lookup(name, root.span)?;
        if var.borrowed {
            return unsupported("assigning into a value that is only borrowed", target.span);
        }

        // `map[key] = value`: the variable hands its map to `set` and gets the result.
        if let ([Step::Index(key, _)], Ty::Map(id)) = (&steps[..], var.ty) {
            let t = self.take_temp(target.span)?;
            let held = self.pending.len();
            let key_ty = self.eval(key, t)?;
            let k = self.push_spill();
            self.a.store(k, t);
            self.hold(k, key_ty);
            self.eval(value, t)?;
            self.pending.truncate(held);
            self.a.mov_rr(R8, t);
            self.a.load(RDX, k);
            self.spill_depth -= 1;
            self.free.push(t);
            self.load_loc(RCX, var.loc);
            let set = self.map_label(MAP_SET, id);
            self.call_label(set);
            self.store_loc(var.loc, RAX);
            return Ok(());
        }

        // The usual cases without leaving the registers: a plain value stored
        // in an item or a field that is reached through fields only
        // (`xs[i] = v`, `p.x = v`, `state.cells[i] = v`).
        let mut fields = Vec::new();
        let mut holder = var.ty;
        let mut quick = true;
        for step in &steps[..steps.len() - 1] {
            let found = match step {
                Step::Field(name, _) => self.fields(holder).into_iter().position(|(field, _)| field == *name),
                Step::Index(..) => None,
            };
            match found {
                Some(i) => {
                    let inner = self.fields(holder)[i].1;
                    fields.push((8 + 8 * i as i32, inner));
                    holder = inner;
                }
                None => {
                    quick = false;
                    break;
                }
            }
        }
        // Where the value goes in the last holder: an item, or a field at an offset.
        let place = match steps.last() {
            Some(Step::Index(index, span)) if quick && matches!(holder, Ty::List(_)) => {
                Some((self.item_ty(holder), Some((*index, *span)), 0))
            }
            Some(Step::Field(name, _)) if quick => self
                .fields(holder)
                .iter()
                .position(|(field, _)| field == *name)
                .map(|i| (self.fields(holder)[i].1, None, 8 + 8 * i as i32)),
            _ => None,
        };
        if let Some((place_ty, index, offset)) = place.filter(|(ty, ..)| !ty.heap() && *ty != Ty::Unit) {
            let _ = place_ty;
            let t = self.take_temp(target.span)?;
            let (a, b, temp, spilled) = match index {
                Some((index, _)) => {
                    if self.eval(index, t)? != Ty::Int {
                        return unsupported("an index that is not an Int", index.span);
                    }
                    let (a, b, _, temp, spilled) = self.operands(t, value)?;
                    (a, b, temp, spilled)
                }
                None => {
                    self.eval(value, t)?;
                    (Opnd::Imm(0), Opnd::Reg(t), None, false)
                }
            };
            let saves = [self.push_spill(), self.push_spill(), self.push_spill()];
            let (unshare, back) = (self.a.label(), self.a.label());
            self.stubs.push(Stub::Unshare {
                label: unshare,
                back,
                variable: var.loc,
                root: var.ty,
                fields: fields.clone(),
                a,
                b,
                saves,
            });
            self.a.bind(back);
            // Every value on the way must be the variable's alone.
            let mut l = self.reg(var.loc, RCX);
            self.a.cmp_imm(M(l, 0), 1);
            self.a.jcc(CC_NE, unshare);
            for (offset, _) in &fields {
                self.a.load(RCX, M(l, *offset));
                l = RCX;
                self.a.cmp_imm(M(l, 0), 1);
                self.a.jcc(CC_NE, unshare);
            }
            match index {
                Some((_, span)) => {
                    // The index and the value are used from where they are
                    // when they are already in registers.
                    let at = match a {
                        Opnd::Reg(r) => r,
                        _ => {
                            self.load_opnd(RAX, a);
                            RAX
                        }
                    };
                    let stub = self.bounds_stub(l, at, span);
                    self.a.alu_m(0x3B, at, M(l, LENGTH));
                    self.a.jcc(CC_AE, stub);
                    match b {
                        Opnd::Reg(r) => self.element(0x89, r, l, at),
                        Opnd::Imm(n) => {
                            // mov qword [l + at * 8 + ELEMENTS], n
                            self.a.b(0x48 | (at >> 3) << 1 | (l >> 3));
                            self.a.b(0xC7);
                            self.a.b(0x44);
                            self.a.b(0xC0 | (at & 7) << 3 | (l & 7));
                            self.a.b(ELEMENTS as u8);
                            self.a.d32(n);
                        }
                        Opnd::Mem(_) => {
                            self.load_opnd(RDX, b);
                            self.element(0x89, RDX, l, at);
                        }
                    }
                }
                None => match b {
                    Opnd::Reg(r) => self.a.store(M(l, offset), r),
                    Opnd::Imm(n) => self.a.store_imm(M(l, offset), n),
                    Opnd::Mem(_) => {
                        self.load_opnd(RDX, b);
                        self.a.store(M(l, offset), RDX);
                    }
                },
            }
            self.spill_depth -= 3;
            self.release(temp, spilled);
            self.free.push(t);
            return Ok(());
        }

        // In general: the indexes and the value are computed and parked,
        // then the way to the place is walked.
        let t = self.take_temp(target.span)?;
        let (held, depth) = (self.pending.len(), self.spill_depth);
        let mut indexes = Vec::new();
        for step in &steps {
            if let Step::Index(index, _) = step {
                if self.eval(index, t)? != Ty::Int {
                    return unsupported("an index that is not an Int", index.span);
                }
                let slot = self.push_spill();
                self.a.store(slot, t);
                indexes.push(slot);
            }
        }
        let value_ty = self.eval(value, t)?;
        let parked = self.push_spill();
        self.a.store(parked, t);
        self.hold(parked, value_ty);
        // The list or struct being looked into, and the offset of the place in it.
        let (container, offset) = (self.push_spill(), self.push_spill());

        // The variable's own value first.
        let unique = self.a.label();
        self.load_loc(RAX, var.loc);
        self.a.cmp_imm(M(RAX, 0), 1);
        self.a.jcc(CC_E, unique);
        self.a.mov_rr(RCX, RAX);
        let clone = self.clone_label(var.ty);
        self.call_label(clone);
        self.load_loc(RCX, var.loc);
        self.a.dec_mem(M(RCX, 0));
        self.store_loc(var.loc, RAX);
        self.a.bind(unique);
        self.a.store(container, RAX);

        let mut ty = var.ty;
        let mut indexes = indexes.into_iter();
        for (i, step) in steps.iter().enumerate() {
            // rdx = the offset of the place in the container.
            let inner = match step {
                Step::Field(name, name_span) => {
                    let layout = self.fields(ty);
                    let Some(at) = layout.iter().position(|(field, _)| field == name) else {
                        return unsupported(format!("assigning to the field `{name}` here"), *name_span);
                    };
                    self.a.mov_imm64(RDX, 8 + 8 * at as i64);
                    layout[at].1
                }
                Step::Index(_, span) => {
                    if !matches!(ty, Ty::List(_)) {
                        return unsupported("assigning to an item of anything but a list", *span);
                    }
                    let slot = indexes.next().expect("one slot per index");
                    self.a.load(RAX, slot);
                    self.a.load(RCX, container);
                    let stub = self.bounds_stub(RCX, RAX, *span);
                    self.a.alu_m(0x3B, RAX, M(RCX, LENGTH));
                    self.a.jcc(CC_AE, stub);
                    self.a.mov_rr(RDX, RAX);
                    self.a.buf.extend_from_slice(&[0x48, 0xC1, 0xE2, 0x03]); // shl rdx, 3
                    self.a.alu_imm(0, RDX, ELEMENTS);
                    self.item_ty(ty)
                }
            };
            if i + 1 < steps.len() {
                // Not there yet: the value at this place is the next
                // container, and must be the variable's alone as well.
                if !matches!(inner, Ty::List(_) | Ty::Struct(_)) {
                    return unsupported("assigning into this kind of value", target.span);
                }
                let unique = self.a.label();
                self.a.store(offset, RDX);
                self.a.load(RCX, container);
                self.a.buf.extend_from_slice(&[0x48, 0x8B, 0x04, 0x11]); // mov rax, [rcx + rdx]
                self.a.cmp_imm(M(RAX, 0), 1);
                self.a.jcc(CC_E, unique);
                self.a.mov_rr(RCX, RAX);
                let clone = self.clone_label(inner);
                self.call_label(clone);
                self.a.load(RCX, container);
                self.a.load(RDX, offset);
                self.a.buf.extend_from_slice(&[0x4C, 0x8B, 0x04, 0x11]); // mov r8, [rcx + rdx]
                self.a.dec_mem(M(R8, 0));
                self.a.buf.extend_from_slice(&[0x48, 0x89, 0x04, 0x11]); // mov [rcx + rdx], rax
                self.a.bind(unique);
                self.a.store(container, RAX);
            } else if inner != Ty::Unit {
                // The place: the new value goes in, the old one is given up.
                self.a.load(RCX, container);
                self.a.buf.extend_from_slice(&[0x48, 0x8B, 0x04, 0x11]); // mov rax, [rcx + rdx]
                self.a.load(R8, parked);
                self.a.buf.extend_from_slice(&[0x4C, 0x89, 0x04, 0x11]); // mov [rcx + rdx], r8
                if inner.heap() {
                    self.a.mov_rr(RCX, RAX);
                    self.drop_value(inner);
                }
            }
            ty = inner;
        }
        self.spill_depth = depth;
        self.pending.truncate(held);
        self.free.push(t);
        Ok(())
    }

    /// `assert l op r`: both sides are kept so that a failure can show them.
    fn assert_cmp(&mut self, op: BinOp, l: &Expr, r: &Expr, text: &str, span: Span) -> R<()> {
        let t = self.take_temp(span)?;
        let held = self.pending.len();
        let ty = self.eval(l, t)?;
        let left = self.push_spill();
        self.a.store(left, t);
        self.hold(left, ty);
        let right_ty = self.eval(r, t)?;
        let right = self.push_spill();
        self.a.store(right, t);
        self.pending.truncate(held);
        if right_ty != ty {
            return unsupported("comparing values of different types", span);
        }
        let d = Diagnostic::new("E0501", format!("assertion failed: `{text}` (left: \u{1}, right: \u{2})"), span);
        let fail = self.report(d, vec![Part::Value(left, ty), Part::Value(right, ty)]);
        let equality = matches!(op, BinOp::Eq | BinOp::Ne);
        match ty {
            Ty::Float => {
                self.a.load(RAX, left);
                self.a.load(RCX, right);
                self.floats_in();
                self.float_compare(op, fail);
            }
            Ty::Str => {
                self.a.load(RCX, left);
                self.a.load(RDX, right);
                self.call_rt(if equality { "str_eq" } else { "str_cmp" });
                self.a.alu_imm(7, RAX, equality as i32);
                self.a.jcc(condition(op) ^ 1, fail);
            }
            _ if self.deep(ty) && equality => {
                let eq = self.eq_label(ty);
                self.a.load(RCX, left);
                self.a.load(RDX, right);
                self.call_label(eq);
                self.a.alu_imm(7, RAX, 1);
                self.a.jcc(condition(op) ^ 1, fail);
            }
            _ if matches!(ty, Ty::Int | Ty::Bool) || self.plain_enum(ty) => {
                self.a.load(RAX, left);
                self.a.alu_m(0x3B, RAX, right);
                self.a.jcc(condition(op) ^ 1, fail);
            }
            _ => return unsupported("comparing values of this type", span),
        }
        if ty.heap() {
            for slot in [left, right] {
                self.a.load(RCX, slot);
                self.drop_value(ty);
            }
        }
        self.spill_depth -= 2;
        self.free.push(t);
        Ok(())
    }

    /// `name = value` for a variable whose value is counted: what it held is given up.
    fn assign_heap(&mut self, name: &str, local: Local, value: &Expr) -> R<()> {
        // `xs = xs.push(v)` hands the variable's own reference to `push`, which
        // can then add the item in place instead of copying the list.
        if let ExprKind::MethodCall { recv, name: method, args, .. } = &value.kind {
            let pushes_itself = matches!(&recv.kind, ExprKind::Ident(n) if n == name);
            if matches!(local.ty, Ty::List(_)) && method == "push" && args.len() == 1 && pushes_itself {
                let t = self.take_temp(value.span)?;
                self.eval(&args[0], t)?;
                // The usual case is written out here: the variable is the only
                // owner and there is room. Everything else goes to `list_push`.
                let (slow, back) = (self.a.label(), self.a.label());
                self.stubs.push(Stub::Push { label: slow, back, variable: local.loc, value: t, list: local.ty });
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
        // `x = f(.., x, ..)`: the function takes over what `x` holds, so that
        // it can change it in place, and `x` gets the result. Only when `x`
        // is read exactly once, as an argument, and nothing can leave in between.
        self.moved = false;
        if let ExprKind::Call { name: callee, local: by_value, args, .. } = &value.kind {
            let is_x = |arg: &Expr| matches!(&arg.kind, ExprKind::Ident(n) if n == name);
            let direct = args.iter().filter(|arg| is_x(arg)).count();
            let elsewhere = args.iter().any(|arg| !is_x(arg) && any_expr(arg, &mut |e| refers(e, name)));
            let leaves = any_expr(value, &mut |e| {
                matches!(e.kind, ExprKind::Return(_) | ExprKind::Try(_) | ExprKind::Break | ExprKind::Continue)
            });
            if direct == 1 && !elsewhere && !leaves && !local.borrowed && !(*by_value && callee == name) {
                self.moving = Some((name.to_string(), false));
            }
        }
        let t = self.take_temp(value.span)?;
        let ty = self.eval(value, t);
        self.moving = None;
        if ty? != local.ty {
            return unsupported("this assignment", value.span);
        }
        if self.moved {
            self.store_loc(local.loc, t);
            self.free.push(t);
            return Ok(());
        }
        self.load_loc(RCX, local.loc);
        self.store_loc(local.loc, t);
        self.free.push(t);
        self.drop_value(local.ty);
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
            ExprKind::Float(f) => {
                self.a.mov_imm64(dst, f.to_bits() as i64);
                Ok(Ty::Float)
            }
            ExprKind::Unit => Ok(Ty::Unit),
            ExprKind::Ident(name) => {
                let local = self.lookup(name, e.span)?;
                self.load_loc(dst, local.loc);
                if self.moving.as_ref().is_some_and(|(source, _)| source == name) {
                    let give_up = self.moving.take().is_some_and(|(_, give_up)| give_up);
                    if local.ty.heap() && !local.borrowed {
                        // The last read of the variable: its reference moves on.
                        if give_up {
                            if let Some(source) = self.scopes.iter_mut().rev().find_map(|s| s.get_mut(name.as_str())) {
                                source.borrowed = true;
                            }
                        }
                        self.moved = true;
                        return Ok(local.ty);
                    }
                }
                self.retain(dst, local.ty);
                Ok(local.ty)
            }
            ExprKind::Str(parts) => self.string(parts, dst),
            ExprKind::List(items) => {
                let ty = self.expr_ty(e)?;
                if !matches!(ty, Ty::List(_)) {
                    return unsupported("this list", e.span);
                }
                self.a.mov_imm64(RCX, items.len() as i64);
                self.call_rt("list_new");
                let slot = self.push_spill();
                self.a.store(slot, RAX);
                let held = self.pending.len();
                self.hold(slot, ty);
                for (i, item) in items.iter().enumerate() {
                    self.eval(item, dst)?;
                    self.a.load(RAX, slot);
                    self.a.store(M(RAX, ELEMENTS + 8 * i as i32), dst);
                    // The length follows the items, so the list is whole at every point.
                    self.a.store_imm(M(RAX, LENGTH), i as i32 + 1);
                }
                self.pending.truncate(held);
                self.a.load(dst, slot);
                self.spill_depth -= 1;
                Ok(ty)
            }
            ExprKind::MapLit(entries) => {
                let ty = self.expr_ty(e)?;
                let Ty::Map(id) = ty else { return unsupported("this map", e.span) };
                self.a.mov_imm64(RCX, entries.len() as i64);
                self.call_rt("map_new");
                let slot = self.push_spill();
                self.a.store(slot, RAX);
                let held = self.pending.len();
                self.hold(slot, ty);
                for (key, value) in entries {
                    let inner = self.pending.len();
                    let key_ty = self.eval(key, dst)?;
                    let k = self.push_spill();
                    self.a.store(k, dst);
                    self.hold(k, key_ty);
                    self.eval(value, dst)?;
                    self.pending.truncate(inner);
                    self.a.mov_rr(R8, dst);
                    self.a.load(RDX, k);
                    self.a.load(RCX, slot);
                    self.spill_depth -= 1;
                    let set = self.map_label(MAP_SET, id);
                    self.call_label(set);
                    self.a.store(slot, RAX);
                }
                self.pending.truncate(held);
                self.a.load(dst, slot);
                self.spill_depth -= 1;
                Ok(ty)
            }
            ExprKind::Index(map, key) if matches!(self.recv_ty(map), Ok(Ty::Map(_))) => {
                // `map[key]`: the value, which must be there.
                let (m, m_owned, key_slot, key_owned, ty) = self.map_find(map, key, dst)?;
                let (key_ty, value_ty) = self.map_def(ty);
                let d = Diagnostic::new("E0507", "key \u{1} is not in the map", e.span)
                    .hint("use `.get(key)` to receive an Option, or check with `.has(key)`");
                let text = self.error_text(&d);
                let (before, after) = text.split_once('\u{1}').unwrap_or((&text, ""));
                let before = (self.constant(before.as_bytes()), before.len());
                let after = (self.constant(after.as_bytes()), after.len());
                let label = self.a.label();
                self.stubs.push(Stub::Missing { label, key: key_slot, ty: key_ty, before, after });
                self.a.alu_rr(0x85, RAX, RAX);
                self.a.jcc(CC_L, label);
                self.a.load(RCX, m);
                self.entry(0x8B, dst, RCX, RAX, MAP_VALUE);
                self.retain(dst, value_ty);
                self.map_done(m, m_owned, key_slot, key_owned, ty, dst);
                Ok(value_ty)
            }
            ExprKind::Index(list, index) => {
                if let Some(list_ty @ Ty::List(_)) = self.path_ty(list) {
                    // Read in place: the variable keeps the list alive.
                    if self.eval(index, dst)? != Ty::Int {
                        return unsupported("an index that is not an Int", index.span);
                    }
                    let l = self.path_reg(list);
                    let stub = self.bounds_stub(l, dst, e.span);
                    self.a.alu_m(0x3B, dst, M(l, LENGTH));
                    // Unsigned, so that a negative index is out of bounds too.
                    self.a.jcc(CC_AE, stub);
                    self.element(0x8B, dst, l, dst);
                    let item = self.item_ty(list_ty);
                    self.retain(dst, item);
                    return Ok(item);
                }
                let list_ty = self.owned_list(list, dst)?;
                let slot = self.push_spill();
                self.a.store(slot, dst);
                let held = self.pending.len();
                self.hold(slot, list_ty);
                if self.eval(index, dst)? != Ty::Int {
                    return unsupported("an index that is not an Int", index.span);
                }
                self.pending.truncate(held);
                self.a.load(RCX, slot);
                self.spill_depth -= 1;
                let stub = self.bounds_stub(RCX, dst, e.span);
                self.a.alu_m(0x3B, dst, M(RCX, LENGTH));
                self.a.jcc(CC_AE, stub);
                self.element(0x8B, dst, RCX, dst);
                let item = self.item_ty(list_ty);
                self.retain(dst, item);
                self.release_keeping(dst, list_ty);
                Ok(item)
            }
            ExprKind::StructLit { fields, .. } => {
                let ty = self.expr_ty(e)?;
                let layout = self.fields(ty);
                // The values first, in the order written, each parked; then
                // the block, with each value in the place the declaration gives it.
                let held = self.pending.len();
                let mut parts = Vec::new();
                for (name, name_span, value) in fields {
                    let Some(at) = layout.iter().position(|(field, _)| field == name) else {
                        return unsupported(format!("the field `{name}`"), *name_span);
                    };
                    let value_ty = self.eval(value, dst)?;
                    let slot = self.push_spill();
                    if value_ty != Ty::Unit {
                        self.a.store(slot, dst);
                        parts.push((slot, 8 + 8 * at as i32));
                    }
                    self.hold(slot, value_ty);
                }
                self.fill_block(self.size_of(ty), &parts, dst);
                self.spill_depth -= fields.len() as i32;
                self.pending.truncate(held);
                Ok(ty)
            }
            ExprKind::Field(base, name, name_span) => {
                let (owned, ty) = match self.path_ty(base) {
                    Some(ty) => (false, ty),
                    None => (true, self.eval(base, dst)?),
                };
                let layout = self.fields(ty);
                let Some(at) = layout.iter().position(|(field, _)| field == name) else {
                    return unsupported(format!("the field `{name}` here"), *name_span);
                };
                let field_ty = layout[at].1;
                let from = if owned {
                    self.a.mov_rr(RCX, dst);
                    RCX
                } else {
                    // Read in place: the variable keeps the struct alive.
                    self.path_reg(base)
                };
                if field_ty != Ty::Unit {
                    self.a.load(dst, M(from, 8 + 8 * at as i32));
                    self.retain(dst, field_ty);
                }
                if owned {
                    self.release_keeping(dst, ty);
                }
                Ok(field_ty)
            }
            ExprKind::Ctor { variant, args, .. } => {
                let ty = self.expr_ty(e)?;
                let variants = self.variants(ty);
                let Some(tag) = variants.iter().position(|(name, _)| name == variant) else {
                    return unsupported(format!("the variant `{variant}`"), e.span);
                };
                if variants[tag].1.is_empty() {
                    // No fields: the number of the variant is the value.
                    self.a.mov_imm64(dst, tag as i64);
                    return Ok(ty);
                }
                let held = self.pending.len();
                let mut parts = Vec::new();
                for (i, arg) in args.iter().enumerate() {
                    let arg_ty = self.eval(arg, dst)?;
                    let slot = self.push_spill();
                    if arg_ty != Ty::Unit {
                        self.a.store(slot, dst);
                        parts.push((slot, 16 + 8 * i as i32));
                    }
                    self.hold(slot, arg_ty);
                }
                self.fill_block(self.size_of(ty), &parts, dst);
                self.a.store_imm(M(dst, 8), tag as i32);
                self.spill_depth -= args.len() as i32;
                self.pending.truncate(held);
                Ok(ty)
            }
            ExprKind::Match { scrutinee, arms } => self.match_(scrutinee, arms, dst, e.span),
            ExprKind::Closure { params, captures, body } => {
                let ty = self.expr_ty(e)?;
                let Some((param_tys, ret)) = self.fn_def(ty) else {
                    return unsupported("this closure", e.span);
                };
                if params.len() != param_tys.len() {
                    return unsupported("this closure", e.span);
                }
                // The captured values are copied into the closure's block.
                let held = self.pending.len();
                let mut parts = Vec::new();
                let mut captured = Vec::new();
                for (i, name) in captures.iter().enumerate() {
                    let local = self.lookup(name, e.span)?;
                    self.load_loc(dst, local.loc);
                    self.retain(dst, local.ty);
                    let slot = self.push_spill();
                    self.a.store(slot, dst);
                    self.hold(slot, local.ty);
                    parts.push((slot, FN_CAPTURES + 8 * i as i32));
                    captured.push((name.clone(), local.ty));
                }
                let bytes = FN_CAPTURES as usize + 8 * captures.len();
                self.fill_block(bytes, &parts, dst);
                self.spill_depth -= captures.len() as i32;
                self.pending.truncate(held);
                let (code, free) = (self.a.label(), self.a.label());
                self.lea_label(RAX, code);
                self.a.store(M(dst, FN_CODE), RAX);
                self.lea_label(RAX, free);
                self.a.store(M(dst, FN_FREE), RAX);
                let params = params.iter().zip(&param_tys).map(|(p, ty)| (p.name.clone(), p.span.start, *ty)).collect();
                let env = self.env.clone();
                self.closures.push(Closure { code, free, bytes, params, captures: captured, ret, body: body.clone(), env });
                Ok(ty)
            }
            ExprKind::FnRef(name) => {
                // A function used as a value: a block with nothing captured,
                // whose code passes the arguments on to the function.
                let key = self.fn_key(name, e.span)?;
                let name = &key;
                let Some(info) = self.fns.get(name) else {
                    return unsupported(format!("the function `{name}` as a value"), e.span);
                };
                if info.params.len() > 3 {
                    return unsupported("a function with more than three parameters as a value", e.span);
                }
                let (params, ret, target) = (info.params.clone(), info.ret, info.label);
                let ty = self.fn_ty(&params, ret);
                let code = match self.thunks.get(name) {
                    Some(label) => *label,
                    None => {
                        let label = self.a.label();
                        self.thunks.insert(name.clone(), label);
                        self.thunk_queue.push((label, target));
                        label
                    }
                };
                let free = match self.plain_free {
                    Some(label) => label,
                    None => {
                        let label = self.a.label();
                        self.plain_free = Some(label);
                        label
                    }
                };
                self.fill_block(FN_CAPTURES as usize, &[], dst);
                self.lea_label(RAX, code);
                self.a.store(M(dst, FN_CODE), RAX);
                self.lea_label(RAX, free);
                self.a.store(M(dst, FN_FREE), RAX);
                Ok(ty)
            }
            ExprKind::Call { name, local: true, args, .. } => {
                // Calling the function value a variable holds. The variable
                // keeps the block alive during the call.
                let local = self.lookup(name, e.span)?;
                let Some((params, ret)) = self.fn_def(local.ty) else {
                    return unsupported(format!("calling `{name}`"), e.span);
                };
                if params.len() != args.len() {
                    return unsupported("this call of a function value", e.span);
                }
                let outer = self.arguments(args, 1)?;
                self.load_loc(RCX, local.loc);
                self.free = outer;
                // Whatever is called may have contracts.
                let site = self.site(e.span);
                self.lea_label(RAX, site);
                self.a.buf.extend_from_slice(&[0xFF, 0x51, FN_CODE as u8]); // call [rcx + FN_CODE]
                if ret != Ty::Unit {
                    self.mov(dst, RAX);
                }
                Ok(ret)
            }
            ExprKind::Try(inner) => {
                let ty = self.eval(inner, dst)?;
                let variants = self.variants(ty);
                if variants.len() != 2 {
                    return unsupported("`?` on this value", e.span);
                }
                let ok = self.a.label();
                let value_ty = if variants[0].1.is_empty() {
                    // Option: `None` is the number 0.
                    self.a.alu_imm(7, dst, 0);
                    self.a.jcc(CC_NE, ok);
                    variants[1].1[0]
                } else {
                    // Result: `Ok` is variant 0.
                    self.a.cmp_imm(M(dst, 8), 0);
                    self.a.jcc(CC_E, ok);
                    variants[0].1[0]
                };
                // `None` or `Err(e)` is the result of the function as it is:
                // it looks the same whatever the type of the other variant.
                self.a.mov_rr(RAX, dst);
                self.unwind(1, 0, Some(RAX));
                self.a.jmp(self.epilogue);
                self.a.bind(ok);
                self.a.mov_rr(RCX, dst);
                if value_ty != Ty::Unit {
                    self.a.load(dst, M(RCX, 16));
                    self.retain(dst, value_ty);
                }
                self.release_keeping(dst, ty);
                Ok(value_ty)
            }
            ExprKind::MethodCall { recv, name, name_span, args } => self.method(e, recv, name, *name_span, args, dst),
            ExprKind::Unary(UnOp::Neg, x) => {
                match self.eval(x, dst)? {
                    Ty::Int => {}
                    Ty::Float => {
                        // Flips the sign bit.
                        self.a.mov_imm64(RAX, i64::MIN);
                        self.a.alu_rr(0x31, dst, RAX);
                        return Ok(Ty::Float);
                    }
                    _ => return unsupported("negating anything but a number", e.span),
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
                self.unwind(1, 0, keep);
                self.a.jmp(self.epilogue);
                Ok(Ty::Never)
            }
            ExprKind::Break | ExprKind::Continue => {
                let Some((exit, next, outside, held)) = self.loops.last().copied() else {
                    return unsupported("`break` outside a loop", e.span);
                };
                self.unwind(outside, held, None);
                self.a.jmp(if matches!(e.kind, ExprKind::Break) { exit } else { next });
                Ok(Ty::Never)
            }
        }
    }

    fn arithmetic(&mut self, op: BinOp, l: &Expr, r: &Expr, dst: u8, span: Span) -> R<Ty> {
        // An Int variable or small literal on the left is used from where it
        // is, and the right side is computed straight into `dst` - unless
        // `dst` is where that variable lives.
        let mut quick = None;
        if let Some((a, Ty::Int)) = self.simple(l) {
            if !matches!(a, Opnd::Reg(reg) if reg == dst) {
                match self.simple(r) {
                    Some((b, Ty::Int)) => quick = Some((a, b)),
                    None if !self.has_call(r) => {
                        if self.eval(r, dst)? != Ty::Int {
                            return unsupported("arithmetic on anything but Ints", span);
                        }
                        quick = Some((a, Opnd::Reg(dst)));
                    }
                    _ => {}
                }
            }
        }
        let (a, b, temp, spilled) = match quick {
            Some((a, b)) => (a, b, None, false),
            None => match self.slow_operands(op, l, r, dst, span)? {
                Some(operands) => operands,
                // Floats were computed there.
                None => return Ok(Ty::Float),
            },
        };
        self.int_arithmetic(op, a, b, dst, span);
        self.release(temp, spilled);
        Ok(Ty::Int)
    }

    /// The general case of `arithmetic`: the left operand goes to `dst`
    /// first. Floats are computed here entirely, and `None` is returned.
    #[allow(clippy::type_complexity)]
    fn slow_operands(&mut self, op: BinOp, l: &Expr, r: &Expr, dst: u8, span: Span) -> R<Option<(Opnd, Opnd, Option<u8>, bool)>> {
        match self.eval(l, dst)? {
            Ty::Int => {}
            Ty::Float => {
                // Floats: no checks; the processor's rules give infinity and "not a number".
                let (a, b, _, temp, spilled) = self.operands(dst, r)?;
                if op == BinOp::Rem {
                    // The exact remainder, as the old floating-point unit computes it.
                    self.load_opnd(RAX, a);
                    self.load_opnd(RCX, b);
                    self.a.store(M(RSP, 0), RCX);
                    self.a.buf.extend_from_slice(&[0xDD, 0x04, 0x24]); // fld qword [rsp]
                    self.a.store(M(RSP, 0), RAX);
                    self.a.buf.extend_from_slice(&[0xDD, 0x04, 0x24]); // fld qword [rsp]
                    let again = self.a.label();
                    self.a.bind(again);
                    self.a.buf.extend_from_slice(&[0xD9, 0xF8]); // fprem
                    self.a.buf.extend_from_slice(&[0xDF, 0xE0]); // fnstsw ax
                    self.a.buf.extend_from_slice(&[0xF6, 0xC4, 0x04]); // test ah, 4 (not finished)
                    self.a.jcc(CC_NZ, again);
                    self.a.buf.extend_from_slice(&[0xDD, 0x1C, 0x24]); // fstp qword [rsp]
                    self.a.buf.extend_from_slice(&[0xDD, 0xD8]); // fstp st(0)
                    self.a.load(dst, M(RSP, 0));
                } else {
                    // Straight from where the operands are, and straight to `dst`.
                    self.float_load(0, a);
                    self.float_load(1, b);
                    let code = match op {
                        BinOp::Add => 0x58,
                        BinOp::Sub => 0x5C,
                        BinOp::Mul => 0x59,
                        _ => 0x5E,
                    };
                    self.a.buf.extend_from_slice(&[0xF2, 0x0F, code, 0xC1]); // addsd / subsd / mulsd / divsd xmm0, xmm1
                    self.a.buf.extend_from_slice(&[0x66, 0x48 | (dst >> 3), 0x0F, 0x7E, 0xC0 | (dst & 7)]); // movq dst, xmm0
                }
                self.release(temp, spilled);
                return Ok(None);
            }
            _ => return unsupported("arithmetic on anything but numbers", span),
        }
        let (a, b, ty, temp, spilled) = self.operands(dst, r)?;
        if ty != Ty::Int {
            return unsupported("arithmetic on anything but Ints", span);
        }
        Ok(Some((a, b, temp, spilled)))
    }

    /// `dst = a op b` for Ints, with the checks.
    fn int_arithmetic(&mut self, op: BinOp, a: Opnd, b: Opnd, dst: u8, span: Span) {
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
    }

    /// `xmm<x> = o` as a float.
    fn float_load(&mut self, x: u8, o: Opnd) {
        let reg = match o {
            Opnd::Reg(r) => r,
            _ => {
                self.load_opnd(RAX, o);
                RAX
            }
        };
        self.a.buf.extend_from_slice(&[0x66, 0x48 | (reg >> 3), 0x0F, 0x6E, 0xC0 | x << 3 | (reg & 7)]); // movq xmm, reg
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
            ExprKind::Binary(op @ (BinOp::Eq | BinOp::Ne), l, r)
                if matches!(r.kind, ExprKind::Int(0)) && divisor_of(l).is_some() && !self.in_runtime =>
            {
                // `x % d == 0` for a constant `d`: no division is needed.
                let ExprKind::Binary(_, x, _) = &l.kind else { return Ok(()) };
                let d = divisor_of(l).unwrap_or(1);
                let (value, owned) = match self.simple(x) {
                    Some((Opnd::Reg(reg), Ty::Int)) => (reg, None),
                    _ => {
                        let t = self.take_temp(e.span)?;
                        if self.eval(x, t)? != Ty::Int {
                            return unsupported("the remainder of anything but Ints", e.span);
                        }
                        (t, Some(t))
                    }
                };
                // Where to go when the number is (`!=`) or is not (`==`) a multiple.
                let wanted = *op == BinOp::Eq;
                if d & (d - 1) == 0 {
                    // A power of two: the low bits are zero.
                    self.a.b(0x48 | (value >> 3));
                    self.a.b(0xF7);
                    self.a.b(0xC0 | (value & 7));
                    self.a.d32((d - 1) as i32); // test value, d - 1
                    self.a.jcc(if wanted { CC_NZ } else { CC_Z }, no);
                } else {
                    let (m, a) = divisibility(d);
                    self.a.mov_rr(RAX, value);
                    self.a.mov_imm64(RCX, m);
                    self.a.imul_rr(RAX, RCX);
                    self.a.mov_imm64(RCX, a);
                    self.a.alu_rr(0x01, RAX, RCX);
                    self.a.mov_imm64(RCX, 2 * a);
                    self.a.alu_rr(0x39, RAX, RCX);
                    self.a.jcc(if wanted { CC_A } else { CC_BE }, no);
                }
                if let Some(t) = owned {
                    self.free.push(t);
                }
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
                if let Some((left, right, lt, _)) = direct {
                    if self.deep(lt) && routine == "str_eq" {
                        let eq = self.eq_label(lt);
                        self.a.mov_rr(RCX, left);
                        self.load_opnd(RDX, right);
                        self.call_label(eq);
                        self.a.alu_imm(7, RAX, 1);
                        self.a.jcc(condition(*op) ^ 1, no);
                        return Ok(());
                    }
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
                        if self.deep(lt) && routine == "str_eq" {
                            // Lists, structs and enums: compared part by part.
                            let slot = self.push_spill();
                            self.a.store(slot, t);
                            self.eq_op((slot, true), r, lt, t)?;
                            self.free.push(t);
                            self.a.alu_imm(7, RAX, 1);
                            self.a.jcc(condition(*op) ^ 1, no);
                            return Ok(());
                        }
                        let (a, b, rt, temp, spilled) = self.operands(t, r)?;
                        (a, b, temp, spilled, Some(t), lt, rt)
                    }
                };
                if lt != rt || !(matches!(lt, Ty::Int | Ty::Bool | Ty::Float) || self.plain_enum(lt)) {
                    return unsupported("comparing values of this type", e.span);
                }
                if lt == Ty::Float {
                    self.float_load(0, left);
                    self.float_load(1, b);
                    self.float_compare(*op, no);
                } else {
                    self.load_opnd(RAX, left);
                    match b {
                        Opnd::Imm(n) => self.a.alu_imm(7, RAX, n),
                        Opnd::Reg(r) => self.a.alu_rr(0x39, RAX, r),
                        Opnd::Mem(m) => self.a.alu_m(0x3B, RAX, m),
                    }
                    self.a.jcc(condition(*op) ^ 1, no);
                }
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
        if name == "panic" && !self.in_runtime {
            // Stops with the message. Nothing is released: the program ends here.
            let (slot, _, ty) = self.park(&args[0], dst)?;
            if ty != Ty::Str {
                return unsupported("`panic` with anything but a String", span);
            }
            let stub = self.report(Diagnostic::new("E0500", "panic: \u{1}", span), vec![Part::Raw(slot)]);
            self.a.jmp(stub);
            self.spill_depth -= 1;
            return Ok(Ty::Never);
        }
        if self.in_runtime && name.starts_with("__") && !self.fns.contains_key(&format!("%{name}")) {
            return self.intrinsic(name, args, dst, span);
        }
        if !self.in_runtime {
            if let Some(ty) = self.os_builtin(name, args, dst, span)? {
                return Ok(ty);
            }
        }
        let key = if self.in_runtime { format!("%{name}") } else { self.fn_key(name, span)? };
        if self.in_runtime {
            self.need_rt(name);
        }
        let Some(info) = self.fns.get(&key) else {
            return unsupported(format!("the function `{name}`"), span);
        };
        if info.params.len() != args.len() {
            return unsupported("a call with the wrong number of arguments", span);
        }
        let (label, ret, contracts) = (info.label, info.ret, info.contracts);

        let outer = self.arguments(args, 0)?;
        self.free = outer;
        if contracts {
            let site = self.site(span);
            self.lea_label(RAX, site);
        }
        self.call_label(label);
        if ret != Ty::Unit {
            self.mov(dst, RAX);
        }
        Ok(ret)
    }

    /// Evaluates the arguments of a call into the argument registers,
    /// starting with the one at `first`. Returns the set of free temps to put
    /// back once the call is made.
    fn arguments(&mut self, args: &[Expr], first: usize) -> R<Vec<u8>> {
        // No value in r8-r11 is live here: whoever holds one parked it before
        // evaluating an expression with a call in it. Each argument goes to
        // its own temp; the temps of later arguments serve as scratch.
        let outer = std::mem::take(&mut self.free);
        let held = self.pending.len();
        if first + args.len() > 4 {
            // More than the registers take: every argument is parked, then
            // the first four go to the registers and the rest to the frame.
            let mut slots = Vec::new();
            for arg in args {
                self.free = TEMPS[1..].to_vec();
                let ty = self.eval(arg, TEMPS[0])?;
                let slot = self.push_spill();
                self.a.store(slot, TEMPS[0]);
                self.hold(slot, ty);
                slots.push(slot);
            }
            self.pending.truncate(held);
            for (i, slot) in slots.iter().enumerate() {
                let k = first + i;
                if k < 4 {
                    self.a.load(ARG_REGS[k], *slot);
                } else {
                    self.a.load(RAX, *slot);
                    self.a.store(M(RSP, 32 + 8 * (k as i32 - 4)), RAX);
                }
            }
            self.spill_depth -= slots.len() as i32;
            return Ok(outer);
        }
        let mut parked: Vec<Option<M>> = vec![None; args.len()];
        let mut arg_tys = Vec::new();
        for (i, arg) in args.iter().enumerate() {
            if self.has_call(arg) {
                // The call inside would overwrite the arguments computed so far.
                for j in 0..i {
                    if parked[j].is_none() {
                        let slot = self.push_spill();
                        self.a.store(slot, ARG_TEMPS[first + j]);
                        parked[j] = Some(slot);
                        self.hold(slot, arg_tys[j]);
                    }
                }
            }
            self.free = ARG_TEMPS[first + i + 1..].iter().copied().filter(|r| TEMPS.contains(r)).collect();
            arg_tys.push(self.eval(arg, ARG_TEMPS[first + i])?);
        }
        self.pending.truncate(held);
        for i in 0..args.len() {
            match parked[i] {
                Some(slot) => self.a.load(ARG_REGS[first + i], slot),
                None => self.mov(ARG_REGS[first + i], ARG_TEMPS[first + i]),
            }
        }
        self.spill_depth -= parked.iter().flatten().count() as i32;
        Ok(outer)
    }

    /// The built-in functions that talk to the operating system: `read_line`,
    /// `args`, `read_file`, `write_file`. `None` if `name` is not one of them.
    fn os_builtin(&mut self, name: &str, args: &[Expr], dst: u8, span: Span) -> R<Option<Ty>> {
        match (name, args) {
            ("read_line", []) => {
                self.call_rt("read_line");
                self.a.mov_rr(dst, RAX);
                Ok(Some(Ty::Str))
            }
            ("args", []) => {
                let ty = self.ty_text("List<String>", &[], span)?;
                self.call_rt("args_list");
                self.a.mov_rr(dst, RAX);
                Ok(Some(ty))
            }
            ("read_file", [path]) => {
                // The routine gives the text, or the message of the error,
                // and says which it is in `ok`.
                let ty = self.ty_text("Result<String, String>", &[], span)?;
                let (p, owned, _) = self.park(path, dst)?;
                let ok = self.push_spill();
                self.a.load(RCX, p);
                self.a.lea(RDX, ok);
                self.call_rt("file_read");
                let text = self.push_spill();
                self.a.store(text, RAX);
                self.fill_block(self.size_of(ty), &[(text, 16)], dst);
                // Ok is variant 0, Err is 1.
                self.a.load(RAX, ok);
                self.a.buf.extend_from_slice(&[0x48, 0x83, 0xF0, 0x01]); // xor rax, 1
                self.a.store(M(dst, 8), RAX);
                if owned {
                    self.a.load(RCX, p);
                    self.release_keeping(dst, Ty::Str);
                }
                self.spill_depth -= 3;
                Ok(Some(ty))
            }
            ("write_file", [path, content]) => {
                // The routine gives 0, or the message of the error.
                let ty = self.ty_text("Result<Unit, String>", &[], span)?;
                let (p, p_owned, _) = self.park(path, dst)?;
                let held = self.pending.len();
                if p_owned {
                    self.hold(p, Ty::Str);
                }
                let (c, c_owned, _) = self.park(content, dst)?;
                self.pending.truncate(held);
                self.a.load(RCX, p);
                self.a.load(RDX, c);
                self.call_rt("file_write");
                let message = self.push_spill();
                self.a.store(message, RAX);
                self.fill_block(self.size_of(ty), &[(message, 16)], dst);
                let ok = self.a.label();
                self.a.store_imm(M(dst, 8), 0);
                self.a.cmp_imm(message, 0);
                self.a.jcc(CC_E, ok);
                self.a.store_imm(M(dst, 8), 1);
                self.a.bind(ok);
                for (slot, owned) in [(p, p_owned), (c, c_owned)] {
                    if owned {
                        self.a.load(RCX, slot);
                        self.release_keeping(dst, Ty::Str);
                    }
                }
                self.spill_depth -= 3;
                Ok(Some(ty))
            }
            ("window_open", [_, _, _, _])
            | ("window_close", [])
            | ("draw_clear", [_])
            | ("draw_pixel", [_, _, _])
            | ("draw_rect", [_, _, _, _, _])
            | ("sleep_ms" | "random_seed", [_]) => {
                self.call_rt_with(name, args, dst)?;
                Ok(Some(Ty::Unit))
            }
            ("rgb", [_, _, _]) | ("time_ms", []) => {
                self.call_rt_with(name, args, dst)?;
                self.a.mov_rr(dst, RAX);
                Ok(Some(Ty::Int))
            }
            ("window_update", []) | ("key_down" | "mouse_down", [_]) => {
                self.call_rt_with(name, args, dst)?;
                self.a.mov_rr(dst, RAX);
                Ok(Some(Ty::Bool))
            }
            ("mouse_x" | "mouse_y", []) => {
                // The place in the window, divided by how large a pixel is shown.
                let (none, done) = (self.a.label(), self.a.label());
                self.a.mov_imm64(RCX, (name == "mouse_y") as i64);
                self.call_rt("mouse_raw");
                self.rip_bss(&[0x48, 0x8B, 0x0D], GLOBALS + 8 * GLOBAL_SCALE); // mov rcx, [scale]
                self.a.alu_imm(7, RCX, 0);
                self.a.jcc(CC_LE, none);
                self.a.buf.extend_from_slice(&[0x48, 0x99]); // cqo
                self.a.buf.extend_from_slice(&[0x48, 0xF7, 0xF9]); // idiv rcx
                self.a.jmp(done);
                self.a.bind(none);
                self.a.mov_imm64(RAX, -1);
                self.a.bind(done);
                self.a.mov_rr(dst, RAX);
                Ok(Some(Ty::Int))
            }
            ("random", [limit]) => {
                // The next number of the sequence, reduced to 0 .. limit - 1.
                self.eval(limit, dst)?;
                let slot = self.push_spill();
                self.a.store(slot, dst);
                self.call_rt("random_next");
                let (zero, done) = (self.a.label(), self.a.label());
                self.a.load(RCX, slot);
                self.spill_depth -= 1;
                self.a.alu_imm(7, RCX, 0);
                self.a.jcc(CC_LE, zero);
                self.a.buf.extend_from_slice(&[0x31, 0xD2]); // xor edx, edx
                self.a.buf.extend_from_slice(&[0x48, 0xF7, 0xF1]); // div rcx
                self.a.mov_rr(RAX, RDX);
                self.a.jmp(done);
                self.a.bind(zero);
                self.a.mov_imm64(RAX, 0);
                self.a.bind(done);
                self.a.mov_rr(dst, RAX);
                Ok(Some(Ty::Int))
            }
            _ => Ok(None),
        }
    }

    /// Calls `routine` of the runtime with the values of `args`, and leaves
    /// its result in rax. Strings and the like are only lent to the routine.
    fn call_rt_with(&mut self, routine: &str, args: &[Expr], dst: u8) -> R<()> {
        let held = self.pending.len();
        let mut parked: Vec<(M, bool, Ty)> = Vec::new();
        for arg in args {
            if let Some((slot, true, ty)) = parked.last().copied() {
                self.hold(slot, ty);
            }
            parked.push(self.park(arg, dst)?);
        }
        self.pending.truncate(held);
        for (i, (slot, ..)) in parked.iter().enumerate() {
            if i < 4 {
                self.a.load(ARG_REGS[i], *slot);
            } else {
                self.a.load(RAX, *slot);
                self.a.store(M(RSP, 32 + 8 * (i as i32 - 4)), RAX);
            }
        }
        self.call_rt(routine);
        for (slot, owned, ty) in &parked {
            if *owned && ty.heap() {
                self.a.load(RCX, *slot);
                self.a.store(*slot, RAX);
                self.drop_value(*ty);
                self.a.load(RAX, *slot);
            }
        }
        self.spill_depth -= parked.len() as i32;
        Ok(())
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
            ("__global", [index]) => {
                let ExprKind::Int(index) = index.kind else {
                    return unsupported("`__global` with anything but a number", span);
                };
                // lea dst, [cell]
                self.rip_bss(&[0x48 | (dst >> 3) << 2, 0x8D, 0x05 | (dst & 7) << 3], GLOBALS + 8 * index as usize);
                Ok(Ty::Int)
            }
            (_, []) if name.starts_with("__import_") => {
                // The address of a function of the operating system.
                let wanted = &name["__import_".len()..];
                let Some(import) = IMPORTS.iter().position(|(_, import)| *import == wanted) else {
                    return unsupported(format!("the import `{wanted}`"), span);
                };
                self.a.buf.extend_from_slice(&[0x48, 0x8B, 0x05]); // mov rax, [rip + import]
                self.iat_fixups.push((self.a.buf.len(), import));
                self.a.d32(0);
                self.a.mov_rr(dst, RAX);
                Ok(Ty::Int)
            }
            ("__load32", [address]) => {
                self.eval(address, dst)?;
                self.a.mov_rr(RAX, dst);
                self.a.buf.extend_from_slice(&[0x8B, 0x00]); // mov eax, [rax]
                self.a.mov_rr(dst, RAX);
                Ok(Ty::Int)
            }
            ("__store32", [address, value]) => {
                self.eval(address, dst)?;
                let (a, b, _, temp, spilled) = self.operands(dst, value)?;
                self.load_opnd(RCX, a);
                self.load_opnd(RAX, b);
                self.a.buf.extend_from_slice(&[0x89, 0x01]); // mov [rcx], eax
                self.release(temp, spilled);
                Ok(Ty::Unit)
            }
            ("__shr" | "__shl" | "__and" | "__xor", [value, by]) => {
                self.eval(value, dst)?;
                let (a, b, _, temp, spilled) = self.operands(dst, by)?;
                self.load_opnd(RAX, a);
                self.load_opnd(RCX, b);
                match name {
                    "__shr" => self.a.buf.extend_from_slice(&[0x48, 0xD3, 0xE8]), // shr rax, cl
                    "__shl" => self.a.buf.extend_from_slice(&[0x48, 0xD3, 0xE0]), // shl rax, cl
                    "__xor" => self.a.buf.extend_from_slice(&[0x48, 0x31, 0xC8]), // xor rax, rcx
                    _ => self.a.buf.extend_from_slice(&[0x48, 0x21, 0xC8]),        // and rax, rcx
                }
                self.a.mov_rr(dst, RAX);
                self.release(temp, spilled);
                Ok(Ty::Int)
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
                StrPart::Expr(value) => match self.quick_ty(value) {
                    Some(Ty::Int) => 20,
                    Some(Ty::Float) => 24,
                    Some(Ty::Bool) => 5,
                    _ => 24,
                },
            })
            .sum();
        self.a.mov_imm64(RCX, room as i64);
        self.call_rt("str_new");
        let building = self.push_spill();
        self.a.store(building, RAX);
        let held = self.pending.len();
        self.hold(building, Ty::Str);
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
                        Ty::Float => {
                            self.a.load(RCX, building);
                            self.a.load(RDX, slot);
                            self.call_rt("str_append_float");
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
                        Ty::List(_) | Ty::Struct(_) | Ty::Enum(_) | Ty::Map(_) => {
                            self.a.load(RCX, building);
                            self.a.load(RDX, slot);
                            self.repr_call(ty);
                            if owned {
                                self.a.store(building, RAX);
                                self.a.load(RCX, slot);
                                self.drop_value(ty);
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
        self.pending.truncate(held);
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

    /// `match`: the value is kept in a place of its own while the arms are
    /// tried in order; what the patterns name is borrowed from it.
    fn match_(&mut self, scrutinee: &Expr, arms: &[Arm], dst: u8, span: Span) -> R<Ty> {
        let outer = self.fresh_temps(dst);
        let root = self.decl_loc[&(span.start, SCRUT)];
        let ty = self.eval_into(scrutinee, root, true)?;
        self.scopes.push(HashMap::from([("%match".to_string(), Local { ty, loc: root, borrowed: false })]));
        let end = self.a.label();
        let mut result = Ty::Never;
        for arm in arms {
            let next = self.a.label();
            self.scopes.push(HashMap::new());
            self.pattern(&arm.pat, root, &mut Vec::new(), ty, next)?;
            let arm_ty = self.eval(&arm.body, dst)?;
            self.scopes.pop();
            if arm_ty != Ty::Never {
                if result == Ty::Never {
                    result = arm_ty;
                }
                self.a.jmp(end);
            }
            self.a.bind(next);
        }
        // The checker made sure that some arm matches, so nothing falls out of the last one.
        self.a.bind(end);
        let keep = (!matches!(result, Ty::Unit | Ty::Never) && TEMPS.contains(&dst)).then_some(dst);
        self.release_scopes(self.scopes.len() - 1, keep);
        self.scopes.pop();
        self.free = outer;
        Ok(result)
    }

    /// Loads into `reg` the part of the value at `root` that `path` leads
    /// to: each step is the offset of a field in the block reached so far.
    fn walk(&mut self, reg: u8, root: Loc, path: &[i32]) {
        self.load_loc(reg, root);
        for offset in path {
            self.a.load(reg, M(reg, *offset));
        }
    }

    /// Jumps to `fail` unless the part of the value at `root` that `path`
    /// leads to, of type `ty`, matches `pat`. Declares the variables of the pattern.
    fn pattern(&mut self, pat: &Pattern, root: Loc, path: &mut Vec<i32>, ty: Ty, fail: usize) -> R<()> {
        match &pat.kind {
            PatKind::Wildcard => {}
            PatKind::Bind(name) => {
                let loc = self.decl_loc[&(pat.span.start, VAR)];
                if ty != Ty::Unit {
                    self.walk(RAX, root, path);
                    self.store_loc(loc, RAX);
                }
                self.declare(name, Local { ty, loc, borrowed: true });
            }
            PatKind::Int(n) => {
                self.walk(RAX, root, path);
                match i32::try_from(*n) {
                    Ok(n) => self.a.alu_imm(7, RAX, n),
                    Err(_) => {
                        self.a.mov_imm64(RCX, *n);
                        self.a.alu_rr(0x39, RAX, RCX);
                    }
                }
                self.a.jcc(CC_NE, fail);
            }
            PatKind::Bool(b) => {
                self.walk(RAX, root, path);
                self.a.alu_imm(7, RAX, *b as i32);
                self.a.jcc(CC_NE, fail);
            }
            PatKind::Str(text) => {
                self.walk(RCX, root, path);
                let constant = self.str_constant(text);
                self.lea_label(RDX, constant);
                self.call_rt("str_eq");
                self.a.alu_imm(7, RAX, 1);
                self.a.jcc(CC_NE, fail);
            }
            PatKind::Ctor { variant, args, .. } => {
                let variants = self.variants(ty);
                let Some(tag) = variants.iter().position(|(name, _)| name == variant) else {
                    return unsupported(format!("the variant `{variant}` in a pattern"), pat.span);
                };
                let fields = &variants[tag].1;
                self.walk(RAX, root, path);
                if fields.is_empty() {
                    self.a.alu_imm(7, RAX, tag as i32);
                    self.a.jcc(CC_NE, fail);
                    return Ok(());
                }
                if variants.iter().any(|(_, fields)| fields.is_empty()) {
                    // A variant without fields is a small number, not a block.
                    self.a.alu_imm(7, RAX, SMALL);
                    self.a.jcc(CC_B, fail);
                }
                self.a.cmp_imm(M(RAX, 8), tag as i32);
                self.a.jcc(CC_NE, fail);
                for (i, arg) in args.iter().enumerate() {
                    let Some(field_ty) = fields.get(i) else {
                        return unsupported("this pattern", arg.span);
                    };
                    path.push(16 + 8 * i as i32);
                    self.pattern(arg, root, path, *field_ty, fail)?;
                    path.pop();
                }
            }
        }
        Ok(())
    }

    /// A method call. Which methods there are depends on the receiver.
    fn method(&mut self, e: &Expr, recv: &Expr, name: &str, name_span: Span, args: &[Expr], dst: u8) -> R<Ty> {
        match (self.recv_ty(recv)?, name, args) {
            (ty @ (Ty::List(_) | Ty::Struct(_) | Ty::Enum(_) | Ty::Map(_) | Ty::Float), "to_string", []) => {
                self.eval(recv, dst)?;
                let slot = self.push_spill();
                self.a.store(slot, dst);
                self.a.mov_imm64(RCX, 32);
                self.call_rt("str_new");
                self.a.mov_rr(RCX, RAX);
                self.a.load(RDX, slot);
                self.repr_call(ty);
                self.a.load(RCX, slot);
                self.a.store(slot, RAX);
                self.drop_value(ty);
                self.a.load(dst, slot);
                self.spill_depth -= 1;
                Ok(Ty::Str)
            }
            (Ty::Float, ..) => self.float_method(e, recv, name, name_span, args, dst),
            (Ty::Int, "to_float", []) => {
                self.eval(recv, dst)?;
                self.a.mov_rr(RAX, dst);
                self.a.buf.extend_from_slice(&[0xF2, 0x48, 0x0F, 0x2A, 0xC0]); // cvtsi2sd xmm0, rax
                self.float_out();
                self.a.mov_rr(dst, RAX);
                Ok(Ty::Float)
            }
            (Ty::Int, "abs" | "min" | "max" | "bit_and" | "bit_or" | "bit_xor" | "shl" | "shr", _) => {
                self.int_method(e, recv, name, name_span, args, dst)
            }
            (Ty::List(_), ..) => self.list_method(e, recv, name, name_span, args, dst),
            (ty @ Ty::Map(_), ..) => self.map_method(e, recv, ty, name, name_span, args, dst),
            (ty @ Ty::Enum(_), ..) => self.enum_method(recv, ty, name, name_span, args, dst),
            (Ty::Str, "to_lower" | "to_upper", []) => {
                // Only the letters a-z and A-Z are converted here; text with
                // anything else beyond ASCII is refused, not converted wrongly.
                let d = Diagnostic::new(
                    "E0902",
                    format!("`{name}` on text outside ASCII is not supported by `--native` yet"),
                    e.span,
                );
                let stub = self.fail_stub(d);
                let (slot, owned, _) = self.park(recv, dst)?;
                self.a.load(RCX, slot);
                self.call_rt("str_is_ascii");
                self.a.alu_rr(0x85, RAX, RAX);
                self.a.jcc(CC_Z, stub);
                self.a.load(RCX, slot);
                self.a.mov_imm64(RDX, if name == "to_lower" { 65 } else { 97 });
                self.call_rt("str_shift_case");
                if owned {
                    self.a.load(RCX, slot);
                    self.a.store(slot, RAX);
                    self.call_rt("str_release");
                    self.a.load(RAX, slot);
                }
                self.spill_depth -= 1;
                self.a.mov_rr(dst, RAX);
                Ok(Ty::Str)
            }
            (Ty::Str, "slice", [from, to]) => self.slice(e, recv, from, to, dst),
            (Ty::Str, "trim", []) => {
                let left = self.park(recv, dst)?;
                self.rt_op("str_trim", left, None, None, None, dst)?;
                self.a.mov_rr(dst, RAX);
                Ok(Ty::Str)
            }
            (Ty::Str, "replace", [from, to]) => {
                // Three strings, each parked; whatever was made for the call is given up after it.
                let held = self.pending.len();
                let mut parked = vec![self.park(recv, dst)?];
                for arg in [from, to] {
                    if let Some((slot, true, ty)) = parked.last().copied() {
                        self.hold(slot, ty);
                    }
                    parked.push(self.park(arg, dst)?);
                }
                self.pending.truncate(held);
                self.a.load(RCX, parked[0].0);
                self.a.load(RDX, parked[1].0);
                self.a.load(R8, parked[2].0);
                self.call_rt("str_replace");
                for (slot, owned, _) in &parked {
                    if *owned {
                        self.a.load(RCX, *slot);
                        self.a.store(*slot, RAX);
                        self.call_rt("str_release");
                        self.a.load(RAX, *slot);
                    }
                }
                self.spill_depth -= 3;
                self.a.mov_rr(dst, RAX);
                Ok(Ty::Str)
            }
            (Ty::Str, "to_int", []) => {
                // The routine says whether the text is a number, and stores the number.
                let result = self.expr_ty(e)?;
                let (slot, owned, _) = self.park(recv, dst)?;
                let value = self.push_spill();
                let (none, done) = (self.a.label(), self.a.label());
                self.a.load(RCX, slot);
                self.a.lea(RDX, value);
                self.call_rt("str_parse_int");
                self.a.alu_rr(0x85, RAX, RAX);
                self.a.jcc(CC_Z, none);
                self.fill_block(self.size_of(result), &[(value, 16)], dst);
                self.a.store_imm(M(dst, 8), 1);
                self.a.jmp(done);
                self.a.bind(none);
                self.a.mov_imm64(dst, 0);
                self.a.bind(done);
                if owned {
                    self.a.load(RCX, slot);
                    self.release_keeping(dst, Ty::Str);
                }
                self.spill_depth -= 2;
                Ok(result)
            }
            (Ty::Int, "to_char", []) => {
                self.eval(recv, dst)?;
                let d = Diagnostic::new("E0509", "\u{1} is not a character code", e.span);
                let stub = self.number_stub(dst, d);
                // Up to 0x10FFFF, and not one of the halves of a pair (0xD800 to 0xDFFF).
                self.a.alu_imm(7, dst, 0x10FFFF);
                self.a.jcc(CC_A, stub);
                self.a.mov_rr(RAX, dst);
                self.a.alu_imm(5, RAX, 0xD800);
                self.a.alu_imm(7, RAX, 0x7FF);
                self.a.jcc(CC_BE, stub);
                self.a.mov_rr(RCX, dst);
                self.call_rt("str_from_code");
                self.a.mov_rr(dst, RAX);
                Ok(Ty::Str)
            }
            (Ty::Str, "split", [_]) | (Ty::Str, "chars" | "codes", []) => {
                let result = self.expr_ty(e)?;
                let left = self.park(recv, dst)?;
                let routine = match name {
                    "split" => "str_split",
                    "chars" => "str_chars_list",
                    _ => "str_codes",
                };
                self.rt_op(routine, left, args.first(), None, None, dst)?;
                self.a.mov_rr(dst, RAX);
                Ok(result)
            }
            (Ty::Str, "to_string", []) => self.eval(recv, dst),
            (Ty::Int, "to_string", []) => {
                self.eval(recv, dst)?;
                self.a.mov_rr(RCX, dst);
                self.call_rt("str_from_int");
                self.a.mov_rr(dst, RAX);
                Ok(Ty::Str)
            }
            (Ty::Bool, "to_string", []) => {
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
            (Ty::Str, ..) => {
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

    /// `mov reg, [map + index * 16 + at]` (0x8B) or the store the other way
    /// (0x89): the key or the value of the entry at `index`. Uses rax, so
    /// `reg` and `map` must be other registers; `index` may be rax.
    fn entry(&mut self, opcode: u8, reg: u8, map: u8, index: u8, at: u8) {
        self.mov(RAX, index);
        self.a.buf.extend_from_slice(&[0x48, 0xC1, 0xE0, 0x04]); // shl rax, 4
        self.a.b(0x48 | (reg >> 3) << 2 | (map >> 3));
        self.a.b(opcode);
        self.a.b(0x44 | (reg & 7) << 3);
        self.a.b(map & 7);
        self.a.b(at);
    }

    fn map_def(&self, ty: Ty) -> (Ty, Ty) {
        match ty {
            Ty::Map(id) => match &self.defs[id as usize] {
                Def::Map(k, v) => (*k, *v),
                _ => (Ty::Never, Ty::Never),
            },
            _ => (Ty::Never, Ty::Never),
        }
    }

    /// The label of generated code for the map type `id` (see `MAP_SET`).
    fn map_label(&mut self, kind: u8, id: u32) -> usize {
        self.uses_heap = true;
        if let Some(label) = self.map_labels.get(&(kind, id)) {
            return *label;
        }
        let label = self.a.label();
        self.map_labels.insert((kind, id), label);
        self.map_queue.push((kind, id));
        label
    }

    /// `set(rcx = map, rdx = key, r8 = value) -> rax` and `remove(rcx = map,
    /// rdx = key) -> rax` for one map type. Both take over the map and give
    /// back the changed one; `set` takes over the key and the value too.
    fn emit_map(&mut self, kind: u8, id: u32) {
        let (key_ty, value_ty) = self.map_def(Ty::Map(id));
        let (unique, absent, done) = (self.a.label(), self.a.label(), self.a.label());
        self.a.bind(self.map_labels[&(kind, id)]);
        self.a.push(RBX);
        self.a.push(RSI);
        self.a.push(RDI);
        self.a.alu_imm(5, RSP, 32);
        self.a.mov_rr(RBX, RCX);
        self.a.mov_rr(RSI, RDX);
        self.a.mov_rr(RDI, R8);
        // A map that is shared is copied first; the other owners keep theirs.
        self.a.cmp_imm(M(RBX, 0), 1);
        self.a.jcc(CC_E, unique);
        self.a.mov_rr(RCX, RBX);
        let clone = self.clone_label(Ty::Map(id));
        self.call_label(clone);
        self.a.dec_mem(M(RBX, 0));
        self.a.mov_rr(RBX, RAX);
        self.a.bind(unique);
        self.a.mov_rr(RCX, RBX);
        self.a.mov_rr(RDX, RSI);
        self.a.mov_imm64(R8, (key_ty == Ty::Str) as i64);
        self.call_rt("map_find");
        self.a.alu_rr(0x85, RAX, RAX);
        self.a.jcc(CC_L, absent);
        if kind == MAP_SET {
            // The key is there: the value is replaced; the old value and the
            // key that was passed are given up.
            self.a.mov_rr(RCX, RBX);
            self.a.mov_rr(RDX, RAX);
            self.a.mov_rr(R8, RDI);
            self.call_rt("map_swap");
            if value_ty.heap() {
                self.a.mov_rr(RCX, RAX);
                self.drop_value(value_ty);
            }
            if key_ty.heap() {
                self.a.mov_rr(RCX, RSI);
                self.drop_value(key_ty);
            }
            self.a.mov_rr(RAX, RBX);
            self.a.jmp(done);
            // Not there: -1 - (where it belongs) came back.
            self.a.bind(absent);
            self.a.buf.extend_from_slice(&[0x48, 0xF7, 0xD0]); // not rax
            self.a.mov_rr(RCX, RBX);
            self.a.mov_rr(RDX, RAX);
            self.a.mov_rr(R8, RSI);
            self.a.mov_rr(R9, RDI);
            self.call_rt("map_insert");
        } else {
            // The entry's key and value are given up, then the entry is closed over.
            self.a.mov_rr(RDI, RAX);
            if key_ty.heap() {
                self.entry(0x8B, RCX, RBX, RDI, MAP_KEY);
                self.drop_value(key_ty);
            }
            if value_ty.heap() {
                self.entry(0x8B, RCX, RBX, RDI, MAP_VALUE);
                self.drop_value(value_ty);
            }
            self.a.mov_rr(RCX, RBX);
            self.a.mov_rr(RDX, RDI);
            self.call_rt("map_close");
            self.a.bind(absent);
            self.a.mov_rr(RAX, RBX);
        }
        self.a.bind(done);
        self.a.alu_imm(0, RSP, 32);
        self.a.pop(RDI);
        self.a.pop(RSI);
        self.a.pop(RBX);
        self.a.b(0xC3);
    }

    /// If `e` is `map.get(key)` on a map: the map and the key.
    fn map_get<'e>(&mut self, e: &'e Expr) -> Option<(&'e Expr, &'e Expr)> {
        match &e.kind {
            ExprKind::MethodCall { recv, name, args, .. } if name == "get" && args.len() == 1 => {
                matches!(self.recv_ty(recv), Ok(Ty::Map(_))).then_some((&**recv, &args[0]))
            }
            _ => None,
        }
    }

    /// Looks `key` up in `map`: parks both (see `park`) and leaves in rax
    /// the index of the entry, or a negative number if there is none.
    fn map_find(&mut self, map: &Expr, key: &Expr, dst: u8) -> R<(M, bool, M, bool, Ty)> {
        let (m, m_owned, ty) = self.park(map, dst)?;
        let held = self.pending.len();
        if m_owned {
            self.hold(m, ty);
        }
        let (k, k_owned, _) = self.park(key, dst)?;
        self.pending.truncate(held);
        let (key_ty, _) = self.map_def(ty);
        self.a.load(RCX, m);
        self.a.load(RDX, k);
        self.a.mov_imm64(R8, (key_ty == Ty::Str) as i64);
        self.call_rt("map_find");
        Ok((m, m_owned, k, k_owned, ty))
    }

    /// After `map_find`: gives up what was parked and frees the slots, without losing `keep`.
    fn map_done(&mut self, m: M, m_owned: bool, k: M, k_owned: bool, ty: Ty, keep: u8) {
        let (key_ty, _) = self.map_def(ty);
        if k_owned && key_ty.heap() {
            self.a.load(RCX, k);
            self.release_keeping(keep, key_ty);
        }
        if m_owned {
            self.a.load(RCX, m);
            self.release_keeping(keep, ty);
        }
        self.spill_depth -= 2;
    }

    /// A method of a map.
    #[allow(clippy::too_many_arguments)]
    fn map_method(&mut self, e: &Expr, recv: &Expr, ty: Ty, name: &str, name_span: Span, args: &[Expr], dst: u8) -> R<Ty> {
        let Ty::Map(id) = ty else { return unsupported("this method", name_span) };
        let (key_ty, value_ty) = self.map_def(ty);
        match (name, args) {
            ("len" | "is_empty", []) => {
                let (slot, owned, _) = self.park(recv, dst)?;
                self.a.load(dst, M(dst, LENGTH));
                if name == "is_empty" {
                    self.a.buf.extend_from_slice(&[0x31, 0xC0]); // xor eax, eax
                    self.a.alu_rr(0x85, dst, dst);
                    self.a.buf.extend_from_slice(&[0x0F, 0x94, 0xC0]); // sete al
                    self.a.mov_rr(dst, RAX);
                }
                if owned {
                    self.a.load(RCX, slot);
                    self.release_keeping(dst, ty);
                }
                self.spill_depth -= 1;
                Ok(if name == "len" { Ty::Int } else { Ty::Bool })
            }
            ("has", [key]) => {
                let (m, m_owned, k, k_owned, _) = self.map_find(recv, key, dst)?;
                // Found if the index is not negative.
                self.a.buf.extend_from_slice(&[0x48, 0xC1, 0xE8, 0x3F]); // shr rax, 63
                self.a.buf.extend_from_slice(&[0x48, 0x83, 0xF0, 0x01]); // xor rax, 1
                self.a.mov_rr(dst, RAX);
                self.map_done(m, m_owned, k, k_owned, ty, dst);
                Ok(Ty::Bool)
            }
            ("get", [key]) => {
                let result = self.expr_ty(e)?;
                let (m, m_owned, k, k_owned, _) = self.map_find(recv, key, dst)?;
                let (none, done) = (self.a.label(), self.a.label());
                self.a.alu_rr(0x85, RAX, RAX);
                self.a.jcc(CC_L, none);
                self.a.load(RCX, m);
                self.entry(0x8B, dst, RCX, RAX, MAP_VALUE);
                self.retain(dst, value_ty);
                let value = self.push_spill();
                self.a.store(value, dst);
                self.fill_block(self.size_of(result), &[(value, 16)], dst);
                self.a.store_imm(M(dst, 8), 1);
                self.spill_depth -= 1;
                self.a.jmp(done);
                self.a.bind(none);
                self.a.mov_imm64(dst, 0);
                self.a.bind(done);
                self.map_done(m, m_owned, k, k_owned, ty, dst);
                Ok(result)
            }
            ("set", [key, value]) => {
                // The map, the key and the value all go to `set`.
                let held = self.pending.len();
                self.eval(recv, dst)?;
                let m = self.push_spill();
                self.a.store(m, dst);
                self.hold(m, ty);
                self.eval(key, dst)?;
                let k = self.push_spill();
                self.a.store(k, dst);
                self.hold(k, key_ty);
                self.eval(value, dst)?;
                self.pending.truncate(held);
                self.a.mov_rr(R8, dst);
                self.a.load(RDX, k);
                self.a.load(RCX, m);
                self.spill_depth -= 2;
                let set = self.map_label(MAP_SET, id);
                self.call_label(set);
                self.a.mov_rr(dst, RAX);
                Ok(ty)
            }
            ("remove", [key]) => {
                let held = self.pending.len();
                self.eval(recv, dst)?;
                let m = self.push_spill();
                self.a.store(m, dst);
                self.hold(m, ty);
                let (k, k_owned, _) = self.park(key, dst)?;
                self.pending.truncate(held);
                self.a.load(RDX, k);
                self.a.load(RCX, m);
                let remove = self.map_label(MAP_REMOVE, id);
                self.call_label(remove);
                self.a.mov_rr(dst, RAX);
                if k_owned && key_ty.heap() {
                    self.a.load(RCX, k);
                    self.release_keeping(dst, key_ty);
                }
                self.spill_depth -= 2;
                Ok(ty)
            }
            ("keys" | "values", []) => {
                let result = self.expr_ty(e)?;
                let left = self.park(recv, dst)?;
                let (routine, item) = if name == "keys" { ("map_keys", key_ty) } else { ("map_values", value_ty) };
                self.rt_op(routine, left, None, None, Some(item), dst)?;
                self.a.mov_rr(dst, RAX);
                Ok(result)
            }
            _ => unsupported(format!("the method `{name}` of a Map"), name_span),
        }
    }

    /// `xmm0 = rax, xmm1 = rcx` as floats.
    fn floats_in(&mut self) {
        self.a.buf.extend_from_slice(&[0x66, 0x48, 0x0F, 0x6E, 0xC0]); // movq xmm0, rax
        self.a.buf.extend_from_slice(&[0x66, 0x48, 0x0F, 0x6E, 0xC9]); // movq xmm1, rcx
    }

    /// `rax = xmm0`.
    fn float_out(&mut self) {
        self.a.buf.extend_from_slice(&[0x66, 0x48, 0x0F, 0x7E, 0xC0]); // movq rax, xmm0
    }

    /// Jumps to `no` unless `xmm0 op xmm1`. A comparison with "not a number"
    /// is false, except `!=`, which is true.
    fn float_compare(&mut self, op: BinOp, no: usize) {
        const A_B: [u8; 4] = [0x66, 0x0F, 0x2E, 0xC1]; // ucomisd xmm0, xmm1
        const B_A: [u8; 4] = [0x66, 0x0F, 0x2E, 0xC8]; // ucomisd xmm1, xmm0
        match op {
            BinOp::Lt => {
                self.a.buf.extend_from_slice(&B_A);
                self.a.jcc(CC_BE, no);
            }
            BinOp::Le => {
                self.a.buf.extend_from_slice(&B_A);
                self.a.jcc(CC_B, no);
            }
            BinOp::Gt => {
                self.a.buf.extend_from_slice(&A_B);
                self.a.jcc(CC_BE, no);
            }
            BinOp::Ge => {
                self.a.buf.extend_from_slice(&A_B);
                self.a.jcc(CC_B, no);
            }
            BinOp::Ne => {
                let yes = self.a.label();
                self.a.buf.extend_from_slice(&A_B);
                self.a.jcc(CC_P, yes);
                self.a.jcc(CC_E, no);
                self.a.bind(yes);
            }
            _ => {
                self.a.buf.extend_from_slice(&A_B);
                self.a.jcc(CC_NE, no);
                self.a.jcc(CC_P, no);
            }
        }
    }

    /// A method of a Float.
    fn float_method(&mut self, e: &Expr, recv: &Expr, name: &str, name_span: Span, args: &[Expr], dst: u8) -> R<Ty> {
        const SIGN: i64 = i64::MIN;
        match (name, args) {
            ("to_int", []) => {
                self.eval(recv, dst)?;
                // Kept where the error message can read it.
                let slot = self.push_spill();
                self.a.store(slot, dst);
                let d = Diagnostic::new("E0503", "`\u{1}` cannot be converted to Int", e.span);
                let text = self.error_text(&d);
                let (before, after) = text.split_once('\u{1}').unwrap_or((&text, ""));
                let before = (self.constant(before.as_bytes()), before.len());
                let after = (self.constant(after.as_bytes()), after.len());
                let stub = self.a.label();
                self.stubs.push(Stub::Missing { label: stub, key: slot, ty: Ty::Float, before, after });
                self.a.mov_rr(RAX, dst);
                // Not a number, infinite, or too large either way.
                self.a.mov_imm64(RCX, 9.3e18f64.to_bits() as i64);
                self.floats_in();
                self.a.buf.extend_from_slice(&[0x66, 0x0F, 0x2E, 0xC1]); // ucomisd xmm0, xmm1
                self.a.jcc(CC_P, stub);
                self.a.jcc(CC_AE, stub);
                self.a.mov_imm64(RCX, (-9.3e18f64).to_bits() as i64);
                self.floats_in();
                self.a.buf.extend_from_slice(&[0x66, 0x0F, 0x2E, 0xC1]); // ucomisd xmm0, xmm1
                self.a.jcc(CC_BE, stub);
                self.a.buf.extend_from_slice(&[0xF2, 0x48, 0x0F, 0x2C, 0xC0]); // cvttsd2si rax, xmm0
                self.a.mov_rr(dst, RAX);
                self.spill_depth -= 1;
                Ok(Ty::Int)
            }
            ("abs", []) => {
                self.eval(recv, dst)?;
                self.a.mov_imm64(RAX, !SIGN);
                self.a.alu_rr(0x21, dst, RAX);
                Ok(Ty::Float)
            }
            ("sqrt" | "floor" | "ceil" | "round", []) => {
                self.eval(recv, dst)?;
                self.a.mov_rr(RAX, dst);
                self.a.buf.extend_from_slice(&[0x66, 0x48, 0x0F, 0x6E, 0xC0]); // movq xmm0, rax
                match name {
                    "sqrt" => self.a.buf.extend_from_slice(&[0xF2, 0x0F, 0x51, 0xC0]), // sqrtsd xmm0, xmm0
                    "floor" => self.a.buf.extend_from_slice(&[0x66, 0x0F, 0x3A, 0x0B, 0xC0, 0x09]), // roundsd down
                    "ceil" => self.a.buf.extend_from_slice(&[0x66, 0x0F, 0x3A, 0x0B, 0xC0, 0x0A]), // roundsd up
                    _ => {
                        // Halves go away from zero: cut the fraction off, then
                        // step one further if at least a half was cut.
                        let done = self.a.label();
                        self.a.buf.extend_from_slice(&[0x66, 0x0F, 0x3A, 0x0B, 0xC8, 0x0B]); // roundsd xmm1, xmm0, toward zero
                        self.a.buf.extend_from_slice(&[0x66, 0x0F, 0x28, 0xD0]); // movapd xmm2, xmm0
                        self.a.buf.extend_from_slice(&[0xF2, 0x0F, 0x5C, 0xD1]); // subsd xmm2, xmm1
                        self.a.buf.extend_from_slice(&[0x66, 0x48, 0x0F, 0x7E, 0xD1]); // movq rcx, xmm2
                        self.a.mov_imm64(RDX, !SIGN);
                        self.a.alu_rr(0x21, RCX, RDX);
                        self.a.mov_imm64(RDX, 0.5f64.to_bits() as i64);
                        self.a.alu_rr(0x39, RCX, RDX);
                        self.a.buf.extend_from_slice(&[0x66, 0x0F, 0x28, 0xC1]); // movapd xmm0, xmm1
                        self.a.jcc(CC_B, done);
                        self.a.mov_rr(RCX, RAX);
                        self.a.mov_imm64(RDX, SIGN);
                        self.a.alu_rr(0x21, RCX, RDX);
                        self.a.mov_imm64(RDX, 1.0f64.to_bits() as i64);
                        self.a.alu_rr(0x09, RCX, RDX);
                        self.a.buf.extend_from_slice(&[0x66, 0x48, 0x0F, 0x6E, 0xD1]); // movq xmm2, rcx
                        self.a.buf.extend_from_slice(&[0xF2, 0x0F, 0x58, 0xC2]); // addsd xmm0, xmm2
                        self.a.bind(done);
                    }
                }
                self.float_out();
                self.a.mov_rr(dst, RAX);
                Ok(Ty::Float)
            }
            ("min" | "max", [other]) => {
                self.eval(recv, dst)?;
                let (a, b, _, temp, spilled) = self.operands(dst, other)?;
                self.load_opnd(RAX, a);
                self.load_opnd(RCX, b);
                self.floats_in();
                let keep = self.a.label();
                self.a.buf.extend_from_slice(&[0x66, 0x0F, 0x28, 0xD0]); // movapd xmm2, xmm0
                // minsd / maxsd xmm0, xmm1: gives the second if either is not a number.
                self.a.buf.extend_from_slice(&[0xF2, 0x0F, if name == "min" { 0x5D } else { 0x5F }, 0xC1]);
                // If the second is not a number, the answer is the first.
                self.a.buf.extend_from_slice(&[0x66, 0x0F, 0x2E, 0xC9]); // ucomisd xmm1, xmm1
                self.a.jcc(CC_NP, keep);
                self.a.buf.extend_from_slice(&[0x66, 0x0F, 0x28, 0xC2]); // movapd xmm0, xmm2
                self.a.bind(keep);
                self.float_out();
                self.a.mov_rr(dst, RAX);
                self.release(temp, spilled);
                Ok(Ty::Float)
            }
            _ => unsupported(format!("the method `{name}` of a Float"), name_span),
        }
    }

    /// A method of an Int.
    fn int_method(&mut self, e: &Expr, recv: &Expr, name: &str, name_span: Span, args: &[Expr], dst: u8) -> R<Ty> {
        match (name, args) {
            ("abs", []) => {
                self.eval(recv, dst)?;
                let stub = self.fail_stub(Diagnostic::new("E0503", "integer overflow in `abs`", e.span));
                let done = self.a.label();
                self.a.mov_rr(RAX, dst);
                self.a.alu_rr(0x85, RAX, RAX);
                self.a.jcc(CC_NS, done);
                self.a.buf.extend_from_slice(&[0x48, 0xF7, 0xD8]); // neg rax
                self.a.jcc(CC_O, stub);
                self.a.bind(done);
                self.a.mov_rr(dst, RAX);
                Ok(Ty::Int)
            }
            (_, [other]) => {
                self.eval(recv, dst)?;
                let (a, b, _, temp, spilled) = self.operands(dst, other)?;
                self.load_opnd(RAX, a);
                self.load_opnd(RCX, b);
                match name {
                    "min" | "max" => {
                        self.a.alu_rr(0x39, RAX, RCX);
                        // cmovg / cmovl rax, rcx
                        self.a.buf.extend_from_slice(&[0x48, 0x0F, if name == "min" { 0x4F } else { 0x4C }, 0xC1]);
                    }
                    "bit_and" => self.a.buf.extend_from_slice(&[0x48, 0x21, 0xC8]), // and rax, rcx
                    "bit_or" => self.a.buf.extend_from_slice(&[0x48, 0x09, 0xC8]),  // or rax, rcx
                    "bit_xor" => self.a.buf.extend_from_slice(&[0x48, 0x31, 0xC8]), // xor rax, rcx
                    _ => {
                        let d = Diagnostic::new("E0509", "shift amount \u{1} is out of range", e.span)
                            .hint("the amount must be between 0 and 63");
                        let stub = self.number_stub(RCX, d);
                        // Unsigned, so that a negative amount is out of range too.
                        self.a.alu_imm(7, RCX, 63);
                        self.a.jcc(CC_A, stub);
                        // shl rax, cl / sar rax, cl: bits shifted out are lost, `shr` keeps the sign.
                        self.a.buf.extend_from_slice(&[0x48, 0xD3, if name == "shl" { 0xE0 } else { 0xF8 }]);
                    }
                }
                self.a.mov_rr(dst, RAX);
                self.release(temp, spilled);
                Ok(Ty::Int)
            }
            _ => unsupported(format!("the method `{name}` here"), name_span),
        }
    }

    /// Whether values of this type are compared and written part by part.
    fn deep(&self, ty: Ty) -> bool {
        matches!(ty, Ty::List(_) | Ty::Struct(_) | Ty::Map(_)) || (matches!(ty, Ty::Enum(_)) && !self.plain_enum(ty))
    }

    /// The label of the generated code that compares two values of type `ty`.
    fn eq_label(&mut self, ty: Ty) -> usize {
        let (Ty::List(id) | Ty::Struct(id) | Ty::Enum(id) | Ty::Map(id)) = ty else { return self.rt_fail };
        self.uses_heap = true;
        if let Some(label) = self.eq_labels.get(&id) {
            return *label;
        }
        let label = self.a.label();
        self.eq_labels.insert(id, label);
        self.text_queue.push((false, id));
        label
    }

    /// Compares the parked value `left` (see `park`) with `right`, both of
    /// type `ty`, and leaves 1 or 0 in rax. Frees the slots and gives up owned references.
    fn eq_op(&mut self, left: (M, bool), right: &Expr, ty: Ty, dst: u8) -> R<()> {
        let held = self.pending.len();
        if left.1 {
            self.hold(left.0, ty);
        }
        let (slot, owned, right_ty) = self.park(right, dst)?;
        self.pending.truncate(held);
        if right_ty != ty {
            return unsupported("comparing values of different types", right.span);
        }
        let eq = self.eq_label(ty);
        self.a.load(RCX, left.0);
        self.a.load(RDX, slot);
        self.call_label(eq);
        for (slot, owned) in [left, (slot, owned)] {
            if owned {
                self.a.load(RCX, slot);
                self.a.store(slot, RAX);
                self.drop_value(ty);
                self.a.load(RAX, slot);
            }
        }
        self.spill_depth -= 2;
        Ok(())
    }

    /// Jumps to `differ` unless the values in rcx and rdx, of type `ty`, are equal.
    fn eq_values(&mut self, ty: Ty, differ: usize) {
        match ty {
            Ty::Unit | Ty::Never => {}
            Ty::Str => {
                self.call_rt("str_eq");
                self.a.alu_imm(7, RAX, 1);
                self.a.jcc(CC_NE, differ);
            }
            Ty::Float => {
                self.a.mov_rr(RAX, RCX);
                self.a.mov_rr(RCX, RDX);
                self.floats_in();
                self.float_compare(BinOp::Eq, differ);
            }
            _ if self.deep(ty) => {
                let eq = self.eq_label(ty);
                self.call_label(eq);
                self.a.alu_imm(7, RAX, 1);
                self.a.jcc(CC_NE, differ);
            }
            _ => {
                self.a.alu_rr(0x39, RCX, RDX);
                self.a.jcc(CC_NE, differ);
            }
        }
    }

    /// `eq(rcx, rdx) -> rax`: 1 if the two values of the type are equal, else 0.
    fn emit_eq(&mut self, id: u32) {
        let def = self.defs[id as usize].clone();
        let (equal, differ, done) = (self.a.label(), self.a.label(), self.a.label());
        self.a.bind(self.eq_labels[&id]);
        self.a.push(RBX);
        self.a.push(RSI);
        self.a.push(RDI);
        self.a.alu_imm(5, RSP, 32);
        self.a.mov_rr(RBX, RCX);
        self.a.mov_rr(RSI, RDX);
        // The same block (or the same variant without fields) is equal to itself.
        self.a.alu_rr(0x39, RBX, RSI);
        self.a.jcc(CC_E, equal);
        match def {
            Def::Struct(fields) => {
                for (i, (_, ty)) in fields.iter().enumerate() {
                    let at = 8 + 8 * i as i32;
                    self.a.load(RCX, M(RBX, at));
                    self.a.load(RDX, M(RSI, at));
                    self.eq_values(*ty, differ);
                }
            }
            Def::Enum(variants) => {
                // Different numbers, or a number and a block: not equal.
                self.a.alu_imm(7, RBX, SMALL);
                self.a.jcc(CC_B, differ);
                self.a.alu_imm(7, RSI, SMALL);
                self.a.jcc(CC_B, differ);
                self.a.load(RAX, M(RBX, 8));
                self.a.alu_m(0x3B, RAX, M(RSI, 8));
                self.a.jcc(CC_NE, differ);
                for (tag, (_, fields)) in variants.iter().enumerate() {
                    if fields.is_empty() {
                        continue;
                    }
                    let next = self.a.label();
                    self.a.cmp_imm(M(RBX, 8), tag as i32);
                    self.a.jcc(CC_NE, next);
                    for (i, ty) in fields.iter().enumerate() {
                        let at = 16 + 8 * i as i32;
                        self.a.load(RCX, M(RBX, at));
                        self.a.load(RDX, M(RSI, at));
                        self.eq_values(*ty, differ);
                    }
                    self.a.jmp(equal);
                    self.a.bind(next);
                }
            }
            Def::List(item) => {
                let top = self.a.label();
                self.a.load(RAX, M(RBX, LENGTH));
                self.a.alu_m(0x3B, RAX, M(RSI, LENGTH));
                self.a.jcc(CC_NE, differ);
                self.a.mov_imm64(RDI, 0);
                self.a.bind(top);
                self.a.alu_m(0x3B, RDI, M(RBX, LENGTH));
                self.a.jcc(CC_GE, equal);
                self.element(0x8B, RCX, RBX, RDI);
                self.element(0x8B, RDX, RSI, RDI);
                self.eq_values(item, differ);
                self.a.alu_imm(0, RDI, 1);
                self.a.jmp(top);
            }
            Def::Map(k, v) => {
                let top = self.a.label();
                self.a.load(RAX, M(RBX, LENGTH));
                self.a.alu_m(0x3B, RAX, M(RSI, LENGTH));
                self.a.jcc(CC_NE, differ);
                self.a.mov_imm64(RDI, 0);
                self.a.bind(top);
                self.a.alu_m(0x3B, RDI, M(RBX, LENGTH));
                self.a.jcc(CC_GE, equal);
                for (at, ty) in [(MAP_KEY, k), (MAP_VALUE, v)] {
                    self.entry(0x8B, RCX, RBX, RDI, at);
                    self.entry(0x8B, RDX, RSI, RDI, at);
                    self.eq_values(ty, differ);
                }
                self.a.alu_imm(0, RDI, 1);
                self.a.jmp(top);
            }
            Def::Fn(..) => self.a.jmp(differ),
        }
        self.a.bind(equal);
        self.a.mov_imm64(RAX, 1);
        self.a.jmp(done);
        self.a.bind(differ);
        self.a.mov_imm64(RAX, 0);
        self.a.bind(done);
        self.a.alu_imm(0, RSP, 32);
        self.a.pop(RDI);
        self.a.pop(RSI);
        self.a.pop(RBX);
        self.a.b(0xC3);
    }

    /// Adds the text of a value, as it appears inside other values (strings
    /// in quotes), to a string being built: rcx = the string, rdx = the
    /// value; the string comes back in rax.
    fn repr_call(&mut self, ty: Ty) {
        match ty {
            Ty::Int => self.call_rt("str_append_int"),
            Ty::Float => self.call_rt("str_append_float"),
            Ty::Str => self.call_rt("str_append_quoted"),
            Ty::Bool => {
                let (yes, no) = (self.constant(b"true"), self.constant(b"false"));
                let (is_false, done) = (self.a.label(), self.a.label());
                self.a.alu_rr(0x85, RDX, RDX);
                self.a.jcc(CC_Z, is_false);
                self.lea_label(RDX, yes);
                self.a.mov_imm64(R8, 4);
                self.a.jmp(done);
                self.a.bind(is_false);
                self.lea_label(RDX, no);
                self.a.mov_imm64(R8, 5);
                self.a.bind(done);
                self.call_rt("str_append");
            }
            Ty::List(id) | Ty::Struct(id) | Ty::Enum(id) | Ty::Map(id) => {
                self.uses_heap = true;
                let label = match self.repr_labels.get(&id) {
                    Some(label) => *label,
                    None => {
                        let label = self.a.label();
                        self.repr_labels.insert(id, label);
                        self.text_queue.push((true, id));
                        label
                    }
                };
                self.call_label(label);
            }
            Ty::Fn(_) => self.repr_text("<closure>", RCX),
            Ty::Unit | Ty::Never => self.repr_text("()", RCX),
        }
    }

    /// Adds fixed text to the string in `string`; the string comes back in rax.
    fn repr_text(&mut self, text: &str, string: u8) {
        let bytes = self.constant(text.as_bytes());
        self.mov(RCX, string);
        self.lea_label(RDX, bytes);
        self.a.mov_imm64(R8, text.len() as i64);
        self.call_rt("str_append");
    }

    /// `repr(rcx = string being built, rdx = value) -> rax`: adds the text
    /// of a value of the type, in the form `yui run` shows it.
    fn emit_repr(&mut self, id: u32) {
        let def = self.defs[id as usize].clone();
        // The name as written, without type arguments.
        let name = self.def_keys[id as usize].split('<').next().unwrap_or("").rsplit('.').next().unwrap_or("").to_string();
        self.a.bind(self.repr_labels[&id]);
        self.a.push(RBX);
        self.a.push(RSI);
        self.a.push(RDI);
        self.a.alu_imm(5, RSP, 32);
        self.a.mov_rr(RBX, RCX);
        self.a.mov_rr(RSI, RDX);
        // rbx = the string, rsi = the value. Each piece: the string in, the string out.
        match def {
            Def::Struct(fields) => {
                self.repr_text(&format!("{name} {{ "), RBX);
                self.a.mov_rr(RBX, RAX);
                for (i, (field, ty)) in fields.iter().enumerate() {
                    let label = if i == 0 { format!("{field}: ") } else { format!(", {field}: ") };
                    self.repr_text(&label, RBX);
                    self.a.mov_rr(RCX, RAX);
                    self.a.load(RDX, M(RSI, 8 + 8 * i as i32));
                    self.repr_call(*ty);
                    self.a.mov_rr(RBX, RAX);
                }
                self.repr_text(" }", RBX);
            }
            Def::Enum(variants) => {
                let (block, known, done) = (self.a.label(), self.a.label(), self.a.label());
                // rdi = the number of the variant.
                self.a.mov_rr(RDI, RSI);
                self.a.alu_imm(7, RSI, SMALL);
                self.a.jcc(CC_B, known);
                self.a.bind(block);
                self.a.load(RDI, M(RSI, 8));
                self.a.bind(known);
                self.a.mov_rr(RAX, RBX);
                for (tag, (variant, fields)) in variants.iter().enumerate() {
                    let next = self.a.label();
                    self.a.alu_imm(7, RDI, tag as i32);
                    self.a.jcc(CC_NE, next);
                    let shown = match name.as_str() {
                        "Option" | "Result" => variant.clone(),
                        _ => format!("{name}.{variant}"),
                    };
                    let shown = if fields.is_empty() { shown } else { format!("{shown}(") };
                    self.repr_text(&shown, RBX);
                    for (i, ty) in fields.iter().enumerate() {
                        if i > 0 {
                            self.repr_text(", ", RAX);
                        }
                        self.a.mov_rr(RCX, RAX);
                        self.a.load(RDX, M(RSI, 16 + 8 * i as i32));
                        self.repr_call(*ty);
                    }
                    if !fields.is_empty() {
                        self.repr_text(")", RAX);
                    }
                    self.a.jmp(done);
                    self.a.bind(next);
                }
                self.a.bind(done);
            }
            Def::List(item) => {
                let (top, first, end) = (self.a.label(), self.a.label(), self.a.label());
                self.repr_text("[", RBX);
                self.a.mov_rr(RBX, RAX);
                self.a.mov_imm64(RDI, 0);
                self.a.bind(top);
                self.a.alu_m(0x3B, RDI, M(RSI, LENGTH));
                self.a.jcc(CC_GE, end);
                self.a.alu_rr(0x85, RDI, RDI);
                self.a.jcc(CC_Z, first);
                self.repr_text(", ", RBX);
                self.a.mov_rr(RBX, RAX);
                self.a.bind(first);
                self.a.mov_rr(RCX, RBX);
                self.element(0x8B, RDX, RSI, RDI);
                self.repr_call(item);
                self.a.mov_rr(RBX, RAX);
                self.a.alu_imm(0, RDI, 1);
                self.a.jmp(top);
                self.a.bind(end);
                self.repr_text("]", RBX);
            }
            Def::Map(k, v) => {
                let (top, first, end, empty, done) =
                    (self.a.label(), self.a.label(), self.a.label(), self.a.label(), self.a.label());
                self.a.cmp_imm(M(RSI, LENGTH), 0);
                self.a.jcc(CC_E, empty);
                self.repr_text("Map { ", RBX);
                self.a.mov_rr(RBX, RAX);
                self.a.mov_imm64(RDI, 0);
                self.a.bind(top);
                self.a.alu_m(0x3B, RDI, M(RSI, LENGTH));
                self.a.jcc(CC_GE, end);
                self.a.alu_rr(0x85, RDI, RDI);
                self.a.jcc(CC_Z, first);
                self.repr_text(", ", RBX);
                self.a.mov_rr(RBX, RAX);
                self.a.bind(first);
                self.a.mov_rr(RCX, RBX);
                self.entry(0x8B, RDX, RSI, RDI, MAP_KEY);
                self.repr_call(k);
                self.repr_text(": ", RAX);
                self.a.mov_rr(RCX, RAX);
                self.entry(0x8B, RDX, RSI, RDI, MAP_VALUE);
                self.repr_call(v);
                self.a.mov_rr(RBX, RAX);
                self.a.alu_imm(0, RDI, 1);
                self.a.jmp(top);
                self.a.bind(end);
                self.repr_text(" }", RBX);
                self.a.jmp(done);
                self.a.bind(empty);
                self.repr_text("Map {}", RBX);
                self.a.bind(done);
            }
            Def::Fn(..) => self.repr_text("<closure>", RBX),
        }
        self.a.alu_imm(0, RSP, 32);
        self.a.pop(RDI);
        self.a.pop(RSI);
        self.a.pop(RBX);
        self.a.b(0xC3);
    }

    /// A method of an `Option` or a `Result`.
    fn enum_method(&mut self, recv: &Expr, ty: Ty, name: &str, name_span: Span, args: &[Expr], dst: u8) -> R<Ty> {
        let variants = self.variants(ty);
        if variants.len() != 2 {
            return unsupported(format!("the method `{name}` here"), name_span);
        }
        // Option: None is the number 0 and Some is variant 1. Result: Ok is variant 0, Err is 1.
        let option = variants[0].1.is_empty();
        let value_ty = if option { variants[1].1[0] } else { variants[0].1[0] };
        match (name, args) {
            ("is_some" | "is_none" | "is_ok" | "is_err", []) => {
                let (slot, owned, _) = self.park(recv, dst)?;
                if option {
                    self.a.buf.extend_from_slice(&[0x31, 0xC0]); // xor eax, eax
                    self.a.alu_rr(0x85, dst, dst);
                    self.a.buf.extend_from_slice(&[0x0F, 0x95, 0xC0]); // setne al
                } else {
                    self.a.load(RAX, M(dst, 8));
                }
                self.a.alu_imm(7, RAX, matches!(name, "is_some" | "is_err") as i32);
                self.a.buf.extend_from_slice(&[0x0F, 0x94, 0xC0]); // sete al
                self.a.buf.extend_from_slice(&[0x0F, 0xB6, 0xC0]); // movzx eax, al
                if owned {
                    self.a.load(RCX, slot);
                    self.a.store(slot, RAX);
                    self.drop_value(ty);
                    self.a.load(RAX, slot);
                }
                self.spill_depth -= 1;
                self.a.mov_rr(dst, RAX);
                Ok(Ty::Bool)
            }
            ("unwrap_or", [default]) if self.map_get(recv).is_some() => {
                // `map.get(key).unwrap_or(default)`: the value is taken
                // straight from the map, without making a `Some` in between.
                let Some((map, key)) = self.map_get(recv) else { return unsupported("this call", name_span) };
                let (m, m_owned, k, k_owned, map_ty) = self.map_find(map, key, dst)?;
                let found = self.push_spill();
                self.a.store(found, RAX);
                let held = self.pending.len();
                if m_owned {
                    self.hold(m, map_ty);
                }
                if k_owned {
                    let (key_ty, _) = self.map_def(map_ty);
                    self.hold(k, key_ty);
                }
                self.eval(default, dst)?;
                self.pending.truncate(held);
                let done = self.a.label();
                self.a.load(RAX, found);
                self.a.alu_rr(0x85, RAX, RAX);
                self.a.jcc(CC_L, done);
                if value_ty.heap() {
                    self.a.mov_rr(RCX, dst);
                    self.drop_value(value_ty);
                }
                self.a.load(RCX, m);
                self.a.load(RAX, found);
                self.entry(0x8B, dst, RCX, RAX, MAP_VALUE);
                self.retain(dst, value_ty);
                self.a.bind(done);
                self.spill_depth -= 1;
                self.map_done(m, m_owned, k, k_owned, map_ty, dst);
                Ok(value_ty)
            }
            ("unwrap_or", [default]) => {
                let (slot, owned, _) = self.park(recv, dst)?;
                // The default is an argument, so it is evaluated either way.
                let held = self.pending.len();
                if owned {
                    self.hold(slot, ty);
                }
                self.eval(default, dst)?;
                self.pending.truncate(held);
                let (has, done) = (self.a.label(), self.a.label());
                self.a.load(RAX, slot);
                if option {
                    self.a.alu_rr(0x85, RAX, RAX);
                    self.a.jcc(CC_NZ, has);
                } else {
                    self.a.cmp_imm(M(RAX, 8), 0);
                    self.a.jcc(CC_E, has);
                }
                self.a.jmp(done);
                self.a.bind(has);
                if value_ty.heap() {
                    self.a.mov_rr(RCX, dst);
                    self.drop_value(value_ty);
                    self.a.load(RAX, slot);
                }
                if value_ty != Ty::Unit {
                    self.a.load(dst, M(RAX, 16));
                    self.retain(dst, value_ty);
                }
                self.a.bind(done);
                if owned {
                    self.a.load(RCX, slot);
                    self.release_keeping(dst, ty);
                }
                self.spill_depth -= 1;
                Ok(value_ty)
            }
            _ => unsupported(format!("the method `{name}` here"), name_span),
        }
    }

    /// Calls `routine` of the runtime on the parked value `left` (see
    /// `park`) and, if given, `right` or the fixed number `extra`; the result
    /// is left in rax. With `fresh`, the result is a new list that holds
    /// items of that type taken from the arguments, which it must count.
    /// Frees the slots and gives up owned references.
    fn rt_op(
        &mut self,
        routine: &str,
        left: (M, bool, Ty),
        right: Option<&Expr>,
        extra: Option<i64>,
        fresh: Option<Ty>,
        dst: u8,
    ) -> R<()> {
        let mut second = None;
        if let Some(right) = right {
            let held = self.pending.len();
            if left.1 {
                self.hold(left.0, left.2);
            }
            second = Some(self.park(right, dst)?);
            self.pending.truncate(held);
        }
        self.a.load(RCX, left.0);
        match (second, extra) {
            (Some((slot, ..)), _) => self.a.load(RDX, slot),
            (None, Some(n)) => self.a.mov_imm64(RDX, n),
            _ => {}
        }
        self.call_rt(routine);
        if let Some(item) = fresh.filter(|item| item.heap()) {
            let (top, done) = (self.a.label(), self.a.label());
            self.a.mov_imm64(RCX, 0);
            self.a.bind(top);
            self.a.alu_m(0x3B, RCX, M(RAX, LENGTH));
            self.a.jcc(CC_GE, done);
            self.element(0x8B, RDX, RAX, RCX);
            self.retain(RDX, item);
            self.a.alu_imm(0, RCX, 1);
            self.a.jmp(top);
            self.a.bind(done);
        }
        for (slot, owned, ty) in [Some(left), second].into_iter().flatten() {
            if owned && ty.heap() {
                self.a.load(RCX, slot);
                self.a.store(slot, RAX);
                self.drop_value(ty);
                self.a.load(RAX, slot);
            }
        }
        self.spill_depth -= 1 + second.is_some() as i32;
        Ok(())
    }

    /// `recv.slice(from, to)` on a list (items) or a string (characters).
    fn slice(&mut self, e: &Expr, recv: &Expr, from: &Expr, to: &Expr, dst: u8) -> R<Ty> {
        let (slot, owned, ty) = self.park(recv, dst)?;
        let held = self.pending.len();
        if owned {
            self.hold(slot, ty);
        }
        self.eval(from, dst)?;
        let a = self.push_spill();
        self.a.store(a, dst);
        self.eval(to, dst)?;
        let b = self.push_spill();
        self.a.store(b, dst);
        self.pending.truncate(held);
        // The length: of a string, in characters.
        self.a.load(RCX, slot);
        if ty == Ty::Str {
            self.call_rt("str_chars");
        } else {
            self.a.load(RAX, M(RCX, LENGTH));
        }
        let len = self.push_spill();
        self.a.store(len, RAX);
        let d = Diagnostic::new("E0505", "slice \u{1}..\u{2} is out of bounds for a length of \u{3}", e.span);
        let fail = self.report(d, vec![Part::Value(a, Ty::Int), Part::Value(b, Ty::Int), Part::Value(len, Ty::Int)]);
        // 0 <= from <= to <= length
        self.a.load(RAX, a);
        self.a.alu_imm(7, RAX, 0);
        self.a.jcc(CC_L, fail);
        self.a.alu_m(0x3B, RAX, b);
        self.a.jcc(CC_G, fail);
        self.a.load(RAX, b);
        self.a.alu_m(0x3B, RAX, len);
        self.a.jcc(CC_G, fail);
        self.a.load(RCX, slot);
        self.a.load(RDX, a);
        self.a.load(R8, b);
        if ty == Ty::Str {
            self.call_rt("str_slice");
        } else {
            self.call_rt("list_slice");
            self.retain_items(self.item_ty(ty));
        }
        if owned {
            self.a.load(RCX, slot);
            self.a.store(slot, RAX);
            self.drop_value(ty);
            self.a.load(RAX, slot);
        }
        self.spill_depth -= 4;
        self.a.mov_rr(dst, RAX);
        Ok(ty)
    }

    /// The list in rax is new and holds items copied from elsewhere: counts them.
    fn retain_items(&mut self, item: Ty) {
        if !item.heap() {
            return;
        }
        let (top, done) = (self.a.label(), self.a.label());
        self.a.mov_imm64(RCX, 0);
        self.a.bind(top);
        self.a.alu_m(0x3B, RCX, M(RAX, LENGTH));
        self.a.jcc(CC_GE, done);
        self.element(0x8B, RDX, RAX, RCX);
        self.retain(RDX, item);
        self.a.alu_imm(0, RCX, 1);
        self.a.jmp(top);
        self.a.bind(done);
    }

    /// A method of a list.
    fn list_method(&mut self, e: &Expr, recv: &Expr, name: &str, name_span: Span, args: &[Expr], dst: u8) -> R<Ty> {
        match (name, args) {
            ("len", []) => {
                if let Some(Ty::List(_)) = self.path_ty(recv) {
                    let l = self.path_reg(recv);
                    self.a.load(dst, M(l, LENGTH));
                } else {
                    let ty = self.owned_list(recv, dst)?;
                    self.a.mov_rr(RCX, dst);
                    self.a.load(dst, M(RCX, LENGTH));
                    self.release_keeping(dst, ty);
                }
                Ok(Ty::Int)
            }
            ("push", [item]) => {
                let ty = self.owned_list(recv, dst)?;
                let slot = self.push_spill();
                self.a.store(slot, dst);
                let held = self.pending.len();
                self.hold(slot, ty);
                self.eval(item, dst)?;
                self.pending.truncate(held);
                self.a.mov_rr(RDX, dst);
                self.a.load(RCX, slot);
                self.spill_depth -= 1;
                self.push_call(ty);
                self.a.mov_rr(dst, RAX);
                Ok(ty)
            }
            ("get", [_]) | ("first" | "last", []) => {
                // `Some(item)` if the index is inside the list, else `None` (the number 0).
                let result = self.expr_ty(e)?;
                let (slot, owned, list_ty) = self.park(recv, dst)?;
                let item = self.item_ty(list_ty);
                match (name, args) {
                    ("get", [index]) => {
                        let held = self.pending.len();
                        if owned {
                            self.hold(slot, list_ty);
                        }
                        self.eval(index, dst)?;
                        self.pending.truncate(held);
                    }
                    ("first", _) => self.a.mov_imm64(dst, 0),
                    _ => {
                        // The last index; for an empty list it is -1, which is outside.
                        self.a.load(RCX, slot);
                        self.a.load(dst, M(RCX, LENGTH));
                        self.a.alu_imm(5, dst, 1);
                    }
                }
                let (none, done) = (self.a.label(), self.a.label());
                self.a.load(RCX, slot);
                self.a.alu_m(0x3B, dst, M(RCX, LENGTH));
                self.a.jcc(CC_AE, none);
                self.element(0x8B, dst, RCX, dst);
                self.retain(dst, item);
                let value = self.push_spill();
                self.a.store(value, dst);
                self.fill_block(self.size_of(result), &[(value, 16)], dst);
                self.a.store_imm(M(dst, 8), 1);
                self.spill_depth -= 1;
                self.a.jmp(done);
                self.a.bind(none);
                self.a.mov_imm64(dst, 0);
                self.a.bind(done);
                if owned {
                    self.a.load(RCX, slot);
                    self.release_keeping(dst, list_ty);
                }
                self.spill_depth -= 1;
                Ok(result)
            }
            ("slice", [from, to]) => self.slice(e, recv, from, to, dst),
            ("is_empty", []) => {
                self.list_method(e, recv, "len", name_span, args, dst)?;
                self.a.buf.extend_from_slice(&[0x31, 0xC0]); // xor eax, eax
                self.a.alu_rr(0x85, dst, dst);
                self.a.buf.extend_from_slice(&[0x0F, 0x94, 0xC0]); // sete al
                self.a.mov_rr(dst, RAX);
                Ok(Ty::Bool)
            }
            ("concat" | "reverse" | "sort" | "join" | "contains", _) => {
                let left = self.park(recv, dst)?;
                let item = self.item_ty(left.2);
                // The routine, a fixed second argument if it takes one, and
                // whether the result is a new list of the same items.
                let (routine, extra, fresh, result) = match (name, args.len(), item) {
                    ("concat", 1, _) => ("list_concat", None, true, left.2),
                    ("reverse", 0, _) => ("list_reversed", None, true, left.2),
                    ("sort", 0, Ty::Int) => ("list_sorted", Some(0), true, left.2),
                    ("sort", 0, Ty::Str) => ("list_sorted", Some(1), true, left.2),
                    ("join", 1, Ty::Str) => ("list_join", None, false, Ty::Str),
                    ("contains", 1, Ty::Int | Ty::Bool) => ("list_contains", None, false, Ty::Bool),
                    ("contains", 1, Ty::Str) => ("list_contains_str", None, false, Ty::Bool),
                    _ => return unsupported(format!("the method `{name}` on this list"), name_span),
                };
                self.rt_op(routine, left, args.first(), extra, fresh.then_some(item), dst)?;
                self.a.mov_rr(dst, RAX);
                Ok(result)
            }
            ("map" | "filter" | "any" | "all", [f]) => self.list_loop(e, recv, name, name_span, None, f, dst),
            ("fold", [start, f]) => self.list_loop(e, recv, name, name_span, Some(start), f, dst),
            _ => unsupported(format!("the method `{name}`"), name_span),
        }
    }

    /// `recv.map(f)` and its relatives where `f` is a function value: a loop
    /// that calls it for each item.
    #[allow(clippy::too_many_arguments)]
    fn list_call_loop(
        &mut self,
        e: &Expr,
        recv: &Expr,
        name: &str,
        name_span: Span,
        start: Option<&Expr>,
        f: &Expr,
        dst: u8,
    ) -> R<Ty> {
        let outer = self.fresh_temps(dst);
        let id = name_span.start;
        let (pos, out, func) = (self.decl_loc[&(id, POS)], self.decl_loc[&(id, OUT)], self.decl_loc[&(id, FUNC)]);
        let (list, owned, list_ty) = match self.list_var(recv) {
            Some(local) => (local.loc, false, local.ty),
            None => {
                let list = self.decl_loc[&(id, LIST)];
                let ty = self.eval_into(recv, list, true)?;
                if !matches!(ty, Ty::List(_)) {
                    return unsupported(format!("`{name}` on anything but a list"), recv.span);
                }
                (list, true, ty)
            }
        };
        let item_ty = self.item_ty(list_ty);
        // Nothing below leaves early, so the list and the function are simply released at the end.
        let fn_ty = self.eval_into(f, func, true)?;
        if self.fn_def(fn_ty).is_none() {
            return unsupported("this argument", f.span);
        }
        let result = match (name, start) {
            ("fold", Some(start)) => {
                let ty = self.eval_into(start, out, false)?;
                if matches!(ty, Ty::Unit | Ty::Never) {
                    return unsupported("`fold` to no value", start.span);
                }
                ty
            }
            ("map" | "filter", _) => {
                let ty = if name == "map" { self.expr_ty(e)? } else { list_ty };
                let l = self.reg(list, RAX);
                self.a.load(RCX, M(l, LENGTH));
                self.call_rt("list_new");
                self.store_loc(out, RAX);
                ty
            }
            _ => {
                self.set_loc(out, (name == "all") as i32);
                Ty::Bool
            }
        };
        self.set_loc(pos, 0);
        let (top, next, exit) = (self.a.label(), self.a.label(), self.a.label());
        self.a.bind(top);
        let l = self.reg(list, RCX);
        let p = self.reg(pos, RAX);
        self.a.alu_m(0x3B, p, M(l, LENGTH));
        self.a.jcc(CC_GE, exit);
        // The function owns its arguments: the item is counted once more, and
        // `fold` hands over the result so far.
        let item = if name == "fold" { R8 } else { RDX };
        self.element(0x8B, item, l, p);
        self.retain(item, item_ty);
        if name == "fold" {
            self.load_loc(RDX, out);
        }
        self.load_loc(RCX, func);
        let site = self.site(e.span);
        self.lea_label(RAX, site);
        self.a.buf.extend_from_slice(&[0xFF, 0x51, FN_CODE as u8]); // call [rcx + FN_CODE]
        match name {
            "map" => {
                let o = self.reg(out, RCX);
                let p = self.reg(pos, RDX);
                self.element(0x89, RAX, o, p);
            }
            "fold" => self.store_loc(out, RAX),
            "filter" => {
                self.a.alu_rr(0x85, RAX, RAX);
                self.a.jcc(CC_Z, next);
                let o = self.reg(out, RCX);
                let l = self.reg(list, RDX);
                let p = self.reg(pos, R8);
                self.element(0x8B, R9, l, p);
                self.retain(R9, item_ty);
                self.a.load(RAX, M(o, LENGTH));
                self.element(0x89, R9, o, RAX);
                self.a.inc_mem(M(o, LENGTH));
            }
            _ => {
                // `any` stops at the first true, `all` at the first false.
                let stop = name == "any";
                self.a.alu_rr(0x85, RAX, RAX);
                self.a.jcc(if stop { CC_Z } else { CC_NZ }, next);
                self.set_loc(out, stop as i32);
                self.a.jmp(exit);
            }
        }
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
            self.drop_value(list_ty);
        }
        self.load_loc(RCX, func);
        self.drop_value(fn_ty);
        self.free = outer;
        self.load_loc(dst, out);
        Ok(result)
    }

    /// `recv.map(|x| ...)` and its relatives, as a loop with the closure's body inside.
    #[allow(clippy::too_many_arguments)]
    fn list_loop(
        &mut self,
        e: &Expr,
        recv: &Expr,
        name: &str,
        name_span: Span,
        start: Option<&Expr>,
        f: &Expr,
        dst: u8,
    ) -> R<Ty> {
        let arity = 1 + start.is_some() as usize;
        let (params, body) = match &f.kind {
            ExprKind::Closure { params, body, .. } if params.len() == arity && inlined(f) => (params, body),
            _ => return self.list_call_loop(e, recv, name, name_span, start, f, dst),
        };
        let outer = self.fresh_temps(dst);
        let id = name_span.start;
        let (pos, out) = (self.decl_loc[&(id, POS)], self.decl_loc[&(id, OUT)]);
        let item = self.decl_loc[&(params[arity - 1].span.start, VAR)];
        // A list held by a variable is walked where it is: the closure cannot
        // assign to that variable. Any other list is this loop's to release.
        let (list, owned, list_ty) = match self.list_var(recv) {
            Some(local) => (local.loc, false, local.ty),
            None => {
                let list = self.decl_loc[&(id, LIST)];
                let ty = self.eval_into(recv, list, true)?;
                if !matches!(ty, Ty::List(_)) {
                    return unsupported(format!("`{name}` on anything but a list"), recv.span);
                }
                (list, true, ty)
            }
        };
        // The closure's parameters are borrowed: the item from the list, and
        // the result so far from the place this loop keeps it in.
        let item_ty = self.item_ty(list_ty);
        let item_local = Local { ty: item_ty, loc: item, borrowed: true };
        let mut scope = HashMap::from([(params[arity - 1].name.clone(), item_local)]);
        let result = match (name, start) {
            ("fold", Some(start)) => {
                let ty = self.eval_into(start, out, false)?;
                if matches!(ty, Ty::Unit | Ty::Never) {
                    return unsupported("`fold` to no value", start.span);
                }
                scope.insert(params[0].name.clone(), Local { ty, loc: out, borrowed: true });
                ty
            }
            ("map" | "filter", _) => {
                let ty = if name == "map" { self.expr_ty(e)? } else { list_ty };
                // Room for every item, so that the loop never has to grow the result.
                let l = self.reg(list, RAX);
                self.a.load(RCX, M(l, LENGTH));
                self.call_rt("list_new");
                self.store_loc(out, RAX);
                ty
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
                if ty != (if name == "map" { self.item_ty(result) } else { result }) {
                    return unsupported("this closure", body.span);
                }
                if name == "map" {
                    let o = self.reg(out, RCX);
                    let p = self.reg(pos, RAX);
                    self.element(0x89, t, o, p);
                } else if result.heap() {
                    // The new result replaces the one so far, which is given up.
                    self.load_loc(RCX, out);
                    self.store_loc(out, t);
                    self.drop_value(result);
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
                // The result holds the item too.
                self.retain(x, item_ty);
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
            self.drop_value(list_ty);
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
            Ty::List(_) | Ty::Struct(_) | Ty::Enum(_) | Ty::Map(_) | Ty::Float => {
                // Written out as text first.
                let slot = self.push_spill();
                self.a.store(slot, t);
                self.a.mov_imm64(RCX, 32);
                self.call_rt("str_new");
                self.a.mov_rr(RCX, RAX);
                self.a.load(RDX, slot);
                self.repr_call(ty);
                let text = self.push_spill();
                self.a.store(text, RAX);
                self.a.lea(RCX, M(RAX, ELEMENTS));
                self.a.load(RDX, M(RAX, LENGTH));
                self.a.mov_imm64(R8, STDOUT);
                self.call_label(self.rt_write);
                self.a.load(RCX, text);
                self.call_rt("str_release");
                self.a.load(RCX, slot);
                self.drop_value(ty);
                self.spill_depth -= 2;
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

/// If `e` is `x % d` for a constant `d` that the quick test for
/// multiples handles (a power of two up to 2^31, or an odd number from 3): `d`.
fn divisor_of(e: &Expr) -> Option<i64> {
    match &e.kind {
        ExprKind::Binary(BinOp::Rem, _, d) => match d.kind {
            ExprKind::Int(d) if d >= 2 && d & (d - 1) == 0 && d <= 1 << 31 => Some(d),
            ExprKind::Int(d) if d >= 3 && d % 2 == 1 => Some(d),
            _ => None,
        },
        _ => None,
    }
}

/// Methods of Int and Float that are compiled to a few instructions in
/// place. No other type has a method with one of these names.
const INLINE_METHODS: [&str; 12] =
    ["abs", "min", "max", "sqrt", "floor", "ceil", "round", "bit_and", "bit_or", "bit_xor", "shl", "shr"];

/// For an odd `d` of at least 3: numbers `(m, a)` such that a signed 64-bit
/// `n` is a multiple of `d` exactly when `n * m + a`, computed with
/// wrap-around and read as unsigned, is at most `2 * a`.
///
/// `m` is the inverse of `d` (`d * m` wraps to 1), so multiplying by it is
/// an exact division for multiples of `d`: they land on the quotients, which
/// lie between `-a` and `a`. Multiplying by `m` is one-to-one, so nothing
/// else can land there.
fn divisibility(d: i64) -> (i64, i64) {
    let mut m = d;
    for _ in 0..6 {
        m = m.wrapping_mul(2i64.wrapping_sub(d.wrapping_mul(m)));
    }
    (m, i64::MAX / d)
}

#[cfg(test)]
mod tests {
    #[test]
    fn divisibility_test_matches_remainder() {
        let mut seed = 0x2545F4914F6CDD1Du64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for d in [3i64, 5, 7, 9, 11, 15, 25, 101, 1001, 65537, 1_000_003, 999_999_999_989, i64::MAX] {
            let (m, a) = super::divisibility(d);
            assert_eq!(d.wrapping_mul(m), 1);
            let check = |n: i64| {
                let quick = (n.wrapping_mul(m).wrapping_add(a) as u64) <= 2 * a as u64;
                assert_eq!(quick, n % d == 0, "{n} % {d}");
            };
            for n in [0, 1, -1, d, -d, d - 1, d.wrapping_add(1), i64::MAX, i64::MIN, i64::MIN + 1, i64::MAX / d * d, -(i64::MAX / d) * d] {
                check(n);
            }
            for k in -2000..2000i64 {
                check(k);
                check(k.wrapping_mul(d));
                check(k.wrapping_mul(d).wrapping_add(1));
            }
            for _ in 0..20000 {
                let n = next() as i64;
                check(n);
                check(n / d * d);
            }
        }
    }
}

/// Whether `test` holds for `e` or anything inside it.
fn any_expr(e: &Expr, test: &mut dyn FnMut(&Expr) -> bool) -> bool {
    if test(e) {
        return true;
    }
    let all = |es: &[Expr], test: &mut dyn FnMut(&Expr) -> bool| es.iter().any(|x| any_expr(x, test));
    match &e.kind {
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Bool(_)
        | ExprKind::Unit
        | ExprKind::Ident(_)
        | ExprKind::FnRef(_)
        | ExprKind::Break
        | ExprKind::Continue
        | ExprKind::Return(None) => false,
        ExprKind::Str(parts) => parts.iter().any(|part| matches!(part, StrPart::Expr(x) if any_expr(x, test))),
        ExprKind::List(items) => all(items, test),
        ExprKind::MapLit(entries) => entries.iter().any(|(k, v)| any_expr(k, test) || any_expr(v, test)),
        ExprKind::Unary(_, x) | ExprKind::Field(x, ..) | ExprKind::Try(x) | ExprKind::Return(Some(x)) => any_expr(x, test),
        ExprKind::Binary(_, l, r) | ExprKind::Index(l, r) => any_expr(l, test) || any_expr(r, test),
        ExprKind::Call { args, .. } | ExprKind::Ctor { args, .. } => all(args, test),
        ExprKind::MethodCall { recv, args, .. } => any_expr(recv, test) || all(args, test),
        ExprKind::StructLit { fields, .. } => fields.iter().any(|(_, _, value)| any_expr(value, test)),
        ExprKind::If { cond, then, els } => {
            any_expr(cond, test) || any_block(then, test) || els.as_ref().is_some_and(|x| any_expr(x, test))
        }
        ExprKind::Match { scrutinee, arms } => {
            any_expr(scrutinee, test) || arms.iter().any(|arm| any_expr(&arm.body, test))
        }
        ExprKind::Block(block) => any_block(block, test),
        ExprKind::Closure { body, .. } => any_expr(body, test),
    }
}

fn any_block(b: &Block, test: &mut dyn FnMut(&Expr) -> bool) -> bool {
    b.stmts.iter().any(|stmt| any_stmt(stmt, test))
}

fn any_stmt(stmt: &Stmt, test: &mut dyn FnMut(&Expr) -> bool) -> bool {
    match stmt {
        Stmt::Let { value, .. } => any_expr(value, test),
        Stmt::Assign { target, value } => any_expr(target, test) || any_expr(value, test),
        Stmt::While { cond, body, .. } => any_expr(cond, test) || any_block(body, test),
        Stmt::For { iter, end, body, .. } => {
            any_expr(iter, test) || end.as_ref().is_some_and(|x| any_expr(x, test)) || any_block(body, test)
        }
        Stmt::Assert { cond, .. } => any_expr(cond, test),
        Stmt::Expr(e) => any_expr(e, test),
    }
}

/// Whether the expression `e` itself (not what is inside it) refers to the variable `name`.
fn refers(e: &Expr, name: &str) -> bool {
    match &e.kind {
        ExprKind::Ident(n) => n == name,
        ExprKind::Call { name: callee, local: true, .. } => callee == name,
        ExprKind::Closure { captures, .. } => captures.iter().any(|c| c == name),
        _ => false,
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
/// any, a third section provides `bss_used` bytes of it, zeroed by the loader.
fn write_pe(mut code: Vec<u8>, iat_fixups: &[(usize, usize)], bss_fixups: &[(usize, usize)], bss_used: usize) -> Vec<u8> {
    const FILE_ALIGN: usize = 0x200;
    const SECTION_ALIGN: usize = 0x1000;
    const IMAGE_BASE: u64 = 0x1_4000_0000;
    const HEADERS: usize = 0x200;
    let text_rva = SECTION_ALIGN;
    let idata_rva = text_rva + align(code.len().max(1), SECTION_ALIGN);

    // The import section: a directory with one entry per library, then for
    // each library a lookup table and an address table (the loader fills the
    // second in), then the names. Only what the code refers to is imported:
    // a program that opens no window does not load the libraries for windows.
    let used: std::collections::BTreeSet<usize> = iat_fixups.iter().map(|(_, import)| *import).collect();
    let mut libraries: Vec<&str> = Vec::new();
    for import in &used {
        if !libraries.contains(&IMPORTS[*import].0) {
            libraries.push(IMPORTS[*import].0);
        }
    }
    let directory = 20 * (libraries.len() + 1);
    let tables = 8 * (used.len() + libraries.len());
    let (lookup, address) = (directory, directory + tables);
    let mut idata = vec![0u8; address + tables];
    let mut slots: HashMap<usize, usize> = HashMap::new();
    let mut at = 0;
    for (n, library) in libraries.iter().enumerate() {
        let (ilt, iat) = (lookup + at, address + at);
        let mut count = 0;
        for import in used.iter().filter(|import| IMPORTS[**import].0 == *library) {
            let name = (idata_rva + idata.len()) as u64;
            idata.extend([0, 0]);
            idata.extend(IMPORTS[*import].1.as_bytes());
            idata.push(0);
            if idata.len() % 2 == 1 {
                idata.push(0);
            }
            idata[ilt + 8 * count..ilt + 8 * count + 8].copy_from_slice(&name.to_le_bytes());
            idata[iat + 8 * count..iat + 8 * count + 8].copy_from_slice(&name.to_le_bytes());
            slots.insert(*import, iat + 8 * count);
            count += 1;
        }
        let library_name = idata_rva + idata.len();
        idata.extend(library.as_bytes());
        idata.push(0);
        if idata.len() % 2 == 1 {
            idata.push(0);
        }
        let entry = 20 * n;
        idata[entry..entry + 4].copy_from_slice(&((idata_rva + ilt) as u32).to_le_bytes());
        idata[entry + 12..entry + 16].copy_from_slice(&(library_name as u32).to_le_bytes());
        idata[entry + 16..entry + 20].copy_from_slice(&((idata_rva + iat) as u32).to_le_bytes());
        at += 8 * (count + 1);
    }

    // Calls to imports are relative to the instruction that follows them.
    for (at, import) in iat_fixups {
        let target = (idata_rva + slots[import]) as i64;
        let rel = (target - (text_rva + at + 4) as i64) as i32;
        code[*at..*at + 4].copy_from_slice(&rel.to_le_bytes());
    }

    let bss_rva = idata_rva + align(idata.len(), SECTION_ALIGN);
    let bss_size = if bss_fixups.is_empty() { 0 } else { align(bss_used, SECTION_ALIGN) };
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
            1 => (idata_rva, directory),
            12 => (idata_rva + address, tables),
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
