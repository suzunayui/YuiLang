//! Ahead-of-time compiler: turns a program into a Windows x64 executable that
//! contains no interpreter (see `docs/native-backend.md`).
//!
//! This is stage 2 of that design. It handles programs made of functions over
//! `Int` and `Bool`: arithmetic, comparisons, `if`, `while`, `for` over a
//! range, calls, and `print` of a string whose `{...}` parts are Ints or
//! Bools. Anything else is reported as `Unsupported`, and the caller falls
//! back to bundling the interpreter.
//!
//! Values have no tags: an Int is 8 bytes in a register. Each function keeps
//! its busiest variables in registers that calls preserve (rbx, rsi, rdi,
//! r12-r15) and the rest in its stack frame. Expressions are evaluated into
//! r8-r11; rax, rcx and rdx are scratch. Functions follow the Windows x64
//! calling convention, so they can call the operating system directly.

use super::{condition, Asm, CC_E, CC_GE, CC_NE, CC_O, M, R10, R11, R12, R13, R14, R15, R8, R9, RAX, RBX, RCX, RDI, RDX, RSI};
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
const IMPORTS: [&str; 3] = ["GetStdHandle", "WriteFile", "ExitProcess"];
const GET_STD_HANDLE: usize = 0;
const WRITE_FILE: usize = 1;
const EXIT_PROCESS: usize = 2;

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
    /// The expression does not produce a value (`return`, `break`, ...).
    Never,
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
}

struct Native<'a> {
    a: Asm,
    sources: &'a SourceMap,
    fns: HashMap<String, FnInfo>,
    /// Positions of `call [rip + ...]` displacements and the import they call.
    iat_fixups: Vec<(usize, usize)>,
    /// Constant data, placed after the code.
    data: Vec<(usize, Vec<u8>)>,
    rt_write: usize,
    rt_print_int: usize,
    rt_fail: usize,
    rt_overflow: usize,

    // ---- state of the function being compiled ----
    stubs: Vec<Stub>,
    scopes: Vec<HashMap<String, Local>>,
    /// Where each declared variable lives, by the start of its declaration.
    decl_loc: HashMap<usize, Loc>,
    /// Where the end of each `for` range lives, by the start of the loop variable.
    limit_loc: HashMap<usize, Loc>,
    free: Vec<u8>,
    /// (label to leave the loop, label to start the next round)
    loops: Vec<(usize, usize)>,
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
    let mut n = Native {
        a,
        sources,
        fns,
        iat_fixups: Vec::new(),
        data: Vec::new(),
        rt_write,
        rt_print_int,
        rt_fail,
        rt_overflow,
        stubs: Vec::new(),
        scopes: Vec::new(),
        decl_loc: HashMap::new(),
        limit_loc: HashMap::new(),
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
    n.a.buf.extend_from_slice(&[0x31, 0xC9]); // xor ecx, ecx
    n.call_import(EXIT_PROCESS);
    n.runtime();
    for f in &decls {
        n.function(f)?;
    }
    for (label, bytes) in std::mem::take(&mut n.data) {
        n.a.bind(label);
        n.a.buf.extend_from_slice(&bytes);
    }
    let iat_fixups = std::mem::take(&mut n.iat_fixups);
    Ok(write_pe(n.a.finish(), &iat_fixups))
}

fn simple_type(t: &TypeExpr) -> R<Ty> {
    match &t.kind {
        TypeKind::Named(name, args) if args.is_empty() => match name.as_str() {
            "Int" => Ok(Ty::Int),
            "Bool" => Ok(Ty::Bool),
            "Unit" => Ok(Ty::Unit),
            other => unsupported(format!("the type `{other}`"), t.span),
        },
        _ => unsupported("this type", t.span),
    }
}

/// Whether evaluating `e` may call a function, which overwrites r8-r11.
/// Anything with statements inside counts, to stay on the safe side.
fn has_call(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Int(_) | ExprKind::Bool(_) | ExprKind::Unit | ExprKind::Ident(_) => false,
        ExprKind::Unary(_, x) => has_call(x),
        ExprKind::Binary(_, l, r) => has_call(l) || has_call(r),
        _ => true,
    }
}

/// How often each variable name is used, counting uses inside loops as more.
fn weigh(e: &Expr, depth: u32, weight: &mut HashMap<String, usize>, decls: &mut Vec<(usize, String, bool)>) {
    match &e.kind {
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

fn weigh_block(b: &Block, depth: u32, weight: &mut HashMap<String, usize>, decls: &mut Vec<(usize, String, bool)>) {
    for stmt in &b.stmts {
        match stmt {
            Stmt::Let { name, name_span, value, .. } => {
                weigh(value, depth, weight, decls);
                decls.push((name_span.start, name.clone(), false));
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
                decls.push((var_span.start, var.clone(), false));
                decls.push((var_span.start, var.clone(), true));
                // The loop itself reads the counter and the end every round.
                *weight.entry(var.clone()).or_default() += 2 * 8usize.saturating_pow((depth + 1).min(6));
                weigh_block(body, depth + 1, weight, decls);
            }
            Stmt::Assert { cond, .. } => weigh(cond, depth, weight, decls),
            Stmt::Expr(e) => weigh(e, depth, weight, decls),
        }
    }
}

impl Native<'_> {
    // ---- small pieces of code ----

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
            }
        }
    }

    /// Reserves code that reports an overflow of `a op b` and returns its label.
    fn overflow_stub(&mut self, op: BinOp, a: Opnd, b: Opnd, span: Span) -> usize {
        // Render the message with markers where the operands go, then cut it there.
        let d = Diagnostic::new("E0503", format!("integer overflow in `\u{1} {} \u{2}`", op.symbol()), span);
        let text = self.error_text(&d);
        let (prefix, rest) = text.split_once('\u{1}').unwrap_or((&text, ""));
        let (middle, suffix) = rest.split_once('\u{2}').unwrap_or((rest, ""));
        let mut record = Vec::new();
        for piece in [prefix, middle, suffix] {
            record.extend((piece.len() as u32).to_le_bytes());
        }
        for piece in [prefix, middle, suffix] {
            record.extend(piece.as_bytes());
        }
        let record = self.constant(&record);
        let label = self.a.label();
        self.stubs.push(Stub::Overflow { label, a, b, record });
        label
    }

    // ---- functions ----

    fn function(&mut self, f: &FnDecl) -> R<()> {
        // Decide where every variable lives: the most used ones in registers.
        let mut weight = HashMap::new();
        let mut decls: Vec<(usize, String, bool)> = f.params.iter().map(|p| (p.span.start, p.name.clone(), false)).collect();
        weigh_block(&f.body, 0, &mut weight, &mut decls);
        let mut order: Vec<usize> = (0..decls.len()).collect();
        order.sort_by_key(|i| std::cmp::Reverse(weight.get(&decls[*i].1).copied().unwrap_or(0)));
        self.decl_loc.clear();
        self.limit_loc.clear();
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
            let (id, _, is_limit) = &decls[i];
            if *is_limit {
                self.limit_loc.insert(*id, loc);
            } else {
                self.decl_loc.insert(*id, loc);
            }
        }
        self.spill_base = 32 + 8 * stack_slots;
        self.spill_depth = 0;
        self.spill_max = 0;
        self.free = TEMPS.to_vec();
        self.loops.clear();
        self.epilogue = self.a.label();
        let info = &self.fns[&f.name];
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
            let loc = self.decl_loc[&p.span.start];
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

    // ---- statements ----

    /// Compiles a block; its value, if any, goes to `dst`.
    fn block(&mut self, b: &Block, dst: u8) -> R<Ty> {
        self.scopes.push(HashMap::new());
        let mut ty = Ty::Unit;
        let last = b.stmts.len().wrapping_sub(1);
        for (i, stmt) in b.stmts.iter().enumerate() {
            ty = match stmt {
                Stmt::Expr(e) if i == last => self.eval(e, dst)?,
                Stmt::Expr(e) => {
                    self.eval(e, dst)?;
                    Ty::Unit
                }
                other => {
                    self.stmt(other)?;
                    Ty::Unit
                }
            };
        }
        self.scopes.pop();
        Ok(ty)
    }

    /// Evaluates `value` into the variable at `loc`.
    fn eval_into(&mut self, value: &Expr, loc: Loc, direct: bool) -> R<Ty> {
        if let (Loc::Reg(r), true) = (loc, direct) {
            return self.eval(value, r);
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
                let loc = self.decl_loc[&name_span.start];
                // The new variable is not visible in its own initializer.
                let ty = self.eval_into(value, loc, true)?;
                if !matches!(ty, Ty::Int | Ty::Bool) {
                    return unsupported("a variable that is not an Int or a Bool", *name_span);
                }
                if name != "_" {
                    self.scopes.last_mut().expect("scope").insert(name.clone(), Local { ty, loc });
                }
            }
            Stmt::Assign { target, value } => {
                let ExprKind::Ident(name) = &target.kind else {
                    return unsupported("assigning to anything but a variable", target.span);
                };
                let local = self.lookup(name, target.span)?;
                // `x = x + 1` can be computed in place: x is read before it is written.
                let updates_itself = match &value.kind {
                    ExprKind::Binary(_, l, r) => {
                        matches!(&l.kind, ExprKind::Ident(n) if n == name) && !mentions(r, name)
                    }
                    _ => false,
                };
                let direct = !has_call(value) && (updates_itself || !mentions(value, name));
                self.eval_into(value, local.loc, direct)?;
            }
            Stmt::While { cond, body, .. } => {
                let (top, exit) = (self.a.label(), self.a.label());
                self.a.bind(top);
                self.cond(cond, exit)?;
                self.loops.push((exit, top));
                let t = self.take_temp(cond.span)?;
                self.block(body, t)?;
                self.free.push(t);
                self.loops.pop();
                self.a.jmp(top);
                self.a.bind(exit);
            }
            Stmt::For { var, var_span, iter, end: Some(end), body, .. } => {
                let counter = self.decl_loc[&var_span.start];
                let limit = self.limit_loc[&var_span.start];
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
                self.loops.push((exit, next));
                let t = self.take_temp(*var_span)?;
                self.block(body, t)?;
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
            Stmt::For { var_span, .. } => return unsupported("`for` over a list", *var_span),
            Stmt::Assert { span, .. } => return unsupported("`assert`", *span),
            Stmt::Expr(e) => {
                let t = self.take_temp(e.span)?;
                self.eval(e, t)?;
                self.free.push(t);
            }
        }
        Ok(())
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
        if has_call(rhs) || self.free.is_empty() {
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
                Ok(local.ty)
            }
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
                if let Some(v) = value {
                    let ty = self.eval(v, dst)?;
                    if self.ret != Ty::Unit && ty != Ty::Never {
                        self.a.mov_rr(RAX, dst);
                    }
                }
                self.a.jmp(self.epilogue);
                Ok(Ty::Never)
            }
            ExprKind::Break | ExprKind::Continue => {
                let Some((exit, next)) = self.loops.last().copied() else {
                    return unsupported("`break` outside a loop", e.span);
                };
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
                let stub = self.overflow_stub(op, a, b, span);
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
                self.a.jcc(CC_O, stub);
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
                    (Some((Opnd::Reg(reg), _)), Some((b, _))) => Some((reg, b)),
                    _ => None,
                };
                let (left, b, temp, spilled, owned) = match direct {
                    Some((reg, b)) => (Opnd::Reg(reg), b, None, false, None),
                    None => {
                        let t = self.take_temp(e.span)?;
                        self.eval(l, t)?;
                        let (a, b, _, temp, spilled) = self.operands(t, r)?;
                        (a, b, temp, spilled, Some(t))
                    }
                };
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
        let Some(info) = self.fns.get(name) else {
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
            if has_call(arg) {
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

    /// `print("text {value} ...")`: writes each piece, then a line break.
    fn print(&mut self, arg: &Expr) -> R<Ty> {
        let ExprKind::Str(parts) = &arg.kind else {
            return unsupported("printing anything but a string literal", arg.span);
        };
        for part in parts {
            match part {
                StrPart::Lit(text) if text.is_empty() => {}
                StrPart::Lit(text) => self.write(text.as_bytes()),
                StrPart::Expr(value) => {
                    let outer = std::mem::replace(&mut self.free, TEMPS[1..].to_vec());
                    let t = TEMPS[0];
                    let ty = self.eval(value, t)?;
                    self.free = outer;
                    match ty {
                        Ty::Int => {
                            self.a.mov_rr(RCX, t);
                            self.a.mov_imm64(RDX, STDOUT);
                            self.call_label(self.rt_print_int);
                        }
                        Ty::Bool => {
                            let (yes, no) = (self.constant(b"true"), self.constant(b"false"));
                            let (is_false, write) = (self.a.label(), self.a.label());
                            self.a.alu_rr(0x85, t, t);
                            self.a.jcc(CC_Z, is_false);
                            self.lea_label(RCX, yes);
                            self.a.mov_imm64(RDX, 4);
                            self.a.jmp(write);
                            self.a.bind(is_false);
                            self.lea_label(RCX, no);
                            self.a.mov_imm64(RDX, 5);
                            self.a.bind(write);
                            self.a.mov_imm64(R8, STDOUT);
                            self.call_label(self.rt_write);
                        }
                        _ => return unsupported("printing a value that is not an Int or a Bool", value.span),
                    }
                }
            }
        }
        self.write(b"\n");
        Ok(Ty::Unit)
    }

    fn write(&mut self, bytes: &[u8]) {
        let text = self.constant(bytes);
        self.lea_label(RCX, text);
        self.a.mov_imm64(RDX, bytes.len() as i64);
        self.a.mov_imm64(R8, STDOUT);
        self.call_label(self.rt_write);
    }
}

/// Whether `e` reads the variable `name`.
fn mentions(e: &Expr, name: &str) -> bool {
    match &e.kind {
        ExprKind::Ident(n) => n == name,
        ExprKind::Int(_) | ExprKind::Bool(_) | ExprKind::Unit => false,
        ExprKind::Unary(_, x) => mentions(x, name),
        ExprKind::Binary(_, l, r) => mentions(l, name) || mentions(r, name),
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
fn write_pe(mut code: Vec<u8>, iat_fixups: &[(usize, usize)]) -> Vec<u8> {
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

    let text_raw = align(code.len(), FILE_ALIGN);
    let idata_raw = align(idata.len(), FILE_ALIGN);
    let image_size = idata_rva + align(idata.len(), SECTION_ALIGN);

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
    u16le(&mut out, 2); // sections
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
    out.resize(HEADERS, 0);
    out.extend(&code);
    out.resize(HEADERS + text_raw, 0);
    out.extend(&idata);
    out.resize(HEADERS + text_raw + idata_raw, 0);
    out
}
