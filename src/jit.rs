//! x86-64 code generator: turns a function's bytecode into machine code.
//!
//! Integer arithmetic, comparisons, branches, counting loops, calls and
//! returns are emitted as native instructions that work directly on the value
//! stack and the frame stack. Every fast path first checks that its operands
//! are the plain values it expects; when they are not, and for every other
//! op, the code calls back into the interpreter to run that single op. The
//! two therefore always agree.
//!
//! The type checker tells the compiler which variables and operations work
//! on `Int`. Those need no checks at all: their values are kept in registers
//! while an expression is evaluated (the "virtual stack"), and the busiest
//! `Int` variables of a function with loops live in registers throughout.
//!
//! Register use inside generated code:
//!   rbx  the machine's `Stack` (see `interp`)
//!   r12  address of the current call's first local slot
//!   r13  address just past the top of the value stack
//!   r14  address just past the innermost frame on the frame stack
//!   r15, rbp  `Int` variables kept in registers
//!   r8-r11, rsi, rdi  values on the virtual stack
//!   rax, rcx, rdx  scratch
//!
//! Helpers use the System V calling convention on every operating system, so
//! one code generator serves them all. Each function starts with a short
//! entry sequence for the interpreter, which saves registers and loads r13
//! and r14 from the `Stack`; generated code calls the body behind it
//! directly, with rbx, r13 and r14 simply staying in place. Keeping the two
//! stack tops in registers matters: a call that stores and reloads them
//! spends most of its time waiting for those memory round trips.

use crate::ast::BinOp;
use crate::bytecode::{Func, InPlace, Op, NONE};

/// Size of a `Value` in bytes.
pub const VALUE_SIZE: usize = 24;
/// Discriminants of the plain values the generated code handles itself.
pub const TAG_INT: u8 = 0;
pub const TAG_BOOL: u8 = 2;
pub const TAG_UNIT: u8 = 3;
/// Values with this discriminant or above own heap memory: their second word
/// points at a reference-counted block.
pub const FIRST_HEAP_TAG: u8 = 4;
pub const TAG_LIST: u8 = 5;
pub const TAG_FN: u8 = 9;
pub const TAG_CLOSURE: u8 = 10;
/// A reference-counted block holds the strong count, the weak count and then the data.
pub const RC_DATA: i32 = 16;
/// Offsets inside `Stack`: the top and end of the value stack, and the top
/// and limit of the frame stack.
pub const STACK_TOP_OFFSET: i32 = 8;
pub const STACK_END_OFFSET: i32 = 16;
pub const FRAMES_TOP_OFFSET: i32 = 40;
pub const FRAMES_LIMIT_OFFSET: i32 = 48;
/// Size of a `Frame` and the offsets of its fields.
pub const FRAME_SIZE: i32 = 40;
pub const FRAME_FUNC: i32 = 0;
pub const FRAME_PC: i32 = 8;
pub const FRAME_BASE: i32 = 16;
pub const FRAME_SPAN: i32 = 24;

/// A call or a return is generated inline only for functions with at most
/// this many local slots to set up or check.
pub const MAX_INLINE_LOCALS: usize = 16;

/// Number of entries in a function's result cache. An entry holds a "filled"
/// word, the arguments and the result. The cache is direct-mapped: a new
/// result simply replaces whatever shared its entry, so it never grows.
pub const MEMO_ENTRIES: usize = 1024;

/// What the generated code needs to know about a function it calls.
pub struct Callee {
    /// Address of the `Func`.
    pub func: usize,
    /// Address of the cell holding the function's machine code address (0
    /// until it has been compiled).
    pub native: usize,
    pub nlocals: usize,
    /// The function returns `Int`, and so also leaves its result in rdx.
    pub int_ret: bool,
    /// Free value-stack cells the function needs before it may be entered.
    pub need: usize,
}

/// What a helper tells the generated code to do next.
pub const NEXT: u32 = 0;
pub const JUMP: u32 = 1;
pub const RETURNED: u32 = 2;
pub const FAILED: u32 = 3;

/// Addresses of the interpreter functions that generated code calls.
pub struct Helpers {
    /// `fn(stack, pc) -> status`: run one op.
    pub op: usize,
    /// `fn(stack, function index, argc, pc) -> entry address or 0`.
    pub call: usize,
    /// `fn(stack, argc, pc) -> entry address or 0`.
    pub call_value: usize,
    /// `fn(stack)`: finish the current call, leaving its result on the stack.
    pub ret: usize,
    /// Offsets of the pointer, length and capacity inside a `Vec<Value>`.
    pub vec_ptr: i32,
    pub vec_len: i32,
    pub vec_cap: i32,
    /// Offsets of the function and the captured values inside a closure.
    pub closure_func: i32,
    pub closure_captured: i32,
}

const V: i32 = VALUE_SIZE as i32;

const RAX: u8 = 0;
const RCX: u8 = 1;
const RDX: u8 = 2;
const RBX: u8 = 3;
const RSI: u8 = 6;
const RDI: u8 = 7;
const RBP: u8 = 5;
const R8: u8 = 8;
const R9: u8 = 9;
const R10: u8 = 10;
const R11: u8 = 11;
const R14: u8 = 14;
const R15: u8 = 15;

/// Registers for values on the virtual stack. All are free to clobber
/// across calls, so the virtual stack is written to memory before any call.
const TEMPS: [u8; 6] = [R8, R9, R10, R11, RSI, RDI];
/// Registers for `Int` variables. Calls preserve them.
const SLOT_REGS: [u8; 2] = [R15, RBP];

/// A value on the virtual stack: known to the compiler but not yet in memory.
#[derive(Clone, Copy)]
enum VItem {
    /// An `Int` in a register from `TEMPS`.
    Reg(u8),
    Imm(i64),
    /// A `Bool` that is true when this condition code holds. Only ever the
    /// top item, and only until the next instruction that changes the flags.
    Cond(u8),
}

/// The op this op may jump to.
fn target_of(op: Op) -> Option<u32> {
    match op {
        Op::Jump(t)
        | Op::JumpIfFalse(t)
        | Op::AndJump(t)
        | Op::OrJump(t)
        | Op::ForRange(_, t)
        | Op::ForRangeNext(_, t)
        | Op::ForList(_, t)
        | Op::JumpUnlessSlotInt(_, _, _, t)
        | Op::JumpUnlessInt(_, _, t) => Some(t),
        Op::Try(t) if t != NONE => Some(t),
        _ => None,
    }
}

/// The local slots an op reads or writes. The flag says whether the compiler
/// handles the op itself when the slot lives in a register; any other use
/// keeps the slot in memory.
fn slot_refs(op: Op) -> Vec<(u32, bool)> {
    match op {
        Op::Load(s) | Op::Store(s) => vec![(s, true)],
        Op::ArithSlotInt(_, s, _) | Op::CmpSlotInt(_, s, _) | Op::JumpUnlessSlotInt(_, s, _, _) => vec![(s, true)],
        Op::ForRange(s, _) | Op::ForRangeNext(s, _) => vec![(s, true), (s + 1, true)],
        Op::ForList(s, _) => vec![(s, false), (s + 1, false), (s + 2, false)],
        Op::Take(s)
        | Op::NewListFor(s)
        | Op::StoreIndex(s)
        | Op::StoreField(s, _)
        | Op::Assign(s, _)
        | Op::InPlace(s, _, _)
        | Op::SaveSp(s)
        | Op::RestoreSp(s)
        | Op::IsVariant(s, _, _)
        | Op::EnumArg(s, _)
        | Op::Ensure(_, s) => vec![(s, false)],
        _ => Vec::new(),
    }
}

/// Picks the `Int` variables that live in registers: the ones used most
/// inside loops. A function without loops keeps everything in memory, since
/// saving and loading the registers would cost more than it gains.
fn choose_slot_regs(func: &Func) -> Vec<Option<u8>> {
    let mut regs = vec![None; func.nlocals];
    let mut weight_at = vec![1usize; func.code.len()];
    let mut has_loop = false;
    for (pc, op) in func.code.iter().enumerate() {
        if let Some(t) = target_of(*op).map(|t| t as usize).filter(|t| *t <= pc) {
            has_loop = true;
            for w in &mut weight_at[t..=pc] {
                *w = w.saturating_mul(8);
            }
        }
    }
    if !has_loop {
        return regs;
    }
    let mut eligible: Vec<bool> = (0..func.nlocals).map(|s| func.int_slots.get(s).copied().unwrap_or(false)).collect();
    let mut weight = vec![0usize; func.nlocals];
    for (pc, op) in func.code.iter().enumerate() {
        for (slot, handled) in slot_refs(*op) {
            let Some(ok) = eligible.get_mut(slot as usize) else { continue };
            if handled {
                weight[slot as usize] = weight[slot as usize].saturating_add(weight_at[pc]);
            } else {
                *ok = false;
            }
        }
    }
    let mut order: Vec<usize> = (0..func.nlocals).filter(|s| eligible[*s] && weight[*s] >= 8).collect();
    order.sort_by_key(|s| std::cmp::Reverse(weight[*s]));
    for (slot, reg) in order.into_iter().zip(SLOT_REGS) {
        regs[slot] = Some(reg);
    }
    regs
}
const R12: u8 = 12;
const R13: u8 = 13;

// Condition codes.
const CC_O: u8 = 0x0;
const CC_B: u8 = 0x2;
const CC_AE: u8 = 0x3;
const CC_E: u8 = 0x4;
const CC_NE: u8 = 0x5;
const CC_BE: u8 = 0x6;
const CC_A: u8 = 0x7;
const CC_L: u8 = 0xC;
const CC_GE: u8 = 0xD;
const CC_LE: u8 = 0xE;
const CC_G: u8 = 0xF;

/// A memory operand: base register and displacement.
#[derive(Clone, Copy)]
struct M(u8, i32);

impl M {
    fn at(self, offset: i32) -> M {
        M(self.0, self.1 + offset)
    }
}

/// The local slot `index`.
fn slot(index: u32) -> M {
    M(R12, index as i32 * V)
}

/// The `n`th value from the top of the stack; `top(0)` is the next free cell.
fn top(n: i32) -> M {
    M(R13, -n * V)
}

fn condition(op: BinOp) -> u8 {
    match op {
        BinOp::Eq => CC_E,
        BinOp::Ne => CC_NE,
        BinOp::Lt => CC_L,
        BinOp::Le => CC_LE,
        BinOp::Gt => CC_G,
        _ => CC_GE,
    }
}

#[derive(Default)]
struct Asm {
    buf: Vec<u8>,
    labels: Vec<Option<usize>>,
    /// Position of a rel32 field and the label it refers to.
    fixups: Vec<(usize, usize)>,
}

impl Asm {
    fn b(&mut self, byte: u8) {
        self.buf.push(byte);
    }

    fn d32(&mut self, v: i32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    fn label(&mut self) -> usize {
        self.labels.push(None);
        self.labels.len() - 1
    }

    fn bind(&mut self, label: usize) {
        self.labels[label] = Some(self.buf.len());
    }

    fn rel32(&mut self, label: usize) {
        self.fixups.push((self.buf.len(), label));
        self.d32(0);
    }

    fn finish(mut self) -> Vec<u8> {
        for (pos, label) in std::mem::take(&mut self.fixups) {
            let target = self.labels[label].expect("label is bound") as i64;
            let rel = (target - (pos as i64 + 4)) as i32;
            self.buf[pos..pos + 4].copy_from_slice(&rel.to_le_bytes());
        }
        self.buf
    }

    /// REX prefix for an instruction with a memory operand.
    fn rex_m(&mut self, wide: bool, reg: u8, m: M) {
        let rex = 0x40 | (wide as u8) << 3 | (reg >> 3) << 2 | (m.0 >> 3);
        if rex != 0x40 {
            self.b(rex);
        }
    }

    fn modrm_m(&mut self, reg: u8, m: M) {
        self.b(0x80 | (reg & 7) << 3 | (m.0 & 7));
        if m.0 & 7 == 4 {
            self.b(0x24);
        }
        self.d32(m.1);
    }

    fn push(&mut self, reg: u8) {
        if reg >= 8 {
            self.b(0x41);
        }
        self.b(0x50 + (reg & 7));
    }

    fn pop(&mut self, reg: u8) {
        if reg >= 8 {
            self.b(0x41);
        }
        self.b(0x58 + (reg & 7));
    }

    fn mov_rr(&mut self, dst: u8, src: u8) {
        self.b(0x48 | (src >> 3) << 2 | (dst >> 3));
        self.b(0x89);
        self.b(0xC0 | (src & 7) << 3 | (dst & 7));
    }

    /// `mov reg, [m]`
    fn load(&mut self, reg: u8, m: M) {
        self.rex_m(true, reg, m);
        self.b(0x8B);
        self.modrm_m(reg, m);
    }

    /// `mov [m], reg`
    fn store(&mut self, m: M, reg: u8) {
        self.rex_m(true, reg, m);
        self.b(0x89);
        self.modrm_m(reg, m);
    }

    /// `mov qword [m], imm32` (sign-extended)
    fn store_imm(&mut self, m: M, imm: i32) {
        self.rex_m(true, 0, m);
        self.b(0xC7);
        self.modrm_m(0, m);
        self.d32(imm);
    }

    fn mov_imm64(&mut self, reg: u8, imm: i64) {
        self.b(0x48 | (reg >> 3));
        self.b(0xB8 + (reg & 7));
        self.buf.extend_from_slice(&imm.to_le_bytes());
    }

    /// `mov reg32, imm32` for one of the low eight registers.
    fn mov_imm32(&mut self, reg: u8, imm: u32) {
        self.b(0xB8 + reg);
        self.buf.extend_from_slice(&imm.to_le_bytes());
    }

    fn lea(&mut self, reg: u8, m: M) {
        self.rex_m(true, reg, m);
        self.b(0x8D);
        self.modrm_m(reg, m);
    }

    /// `add`, `sub` or `cmp reg, [m]`, chosen by opcode.
    fn alu_m(&mut self, opcode: u8, reg: u8, m: M) {
        self.rex_m(true, reg, m);
        self.b(opcode);
        self.modrm_m(reg, m);
    }

    fn imul_m(&mut self, reg: u8, m: M) {
        self.rex_m(true, reg, m);
        self.b(0x0F);
        self.b(0xAF);
        self.modrm_m(reg, m);
    }

    /// `add` (0), `sub` (5) or `cmp` (7) `reg, imm32`.
    fn alu_imm(&mut self, ext: u8, reg: u8, imm: i32) {
        self.b(0x48 | (reg >> 3));
        self.b(0x81);
        self.b(0xC0 | ext << 3 | (reg & 7));
        self.d32(imm);
    }

    fn imul_imm(&mut self, reg: u8, imm: i32) {
        self.b(0x48 | (reg >> 3) << 2 | (reg >> 3));
        self.b(0x69);
        self.b(0xC0 | (reg & 7) << 3 | (reg & 7));
        self.d32(imm);
    }

    /// `cmp byte [m], imm8`
    fn cmp_byte(&mut self, m: M, imm: u8) {
        self.rex_m(false, 7, m);
        self.b(0x80);
        self.modrm_m(7, m);
        self.b(imm);
    }

    /// `cmp qword [m], imm32`
    fn cmp_imm(&mut self, m: M, imm: i32) {
        self.alu_mem_imm(7, m, imm);
    }

    /// `add` (0), `sub` (5) or `cmp` (7) `qword [m], imm32`.
    fn alu_mem_imm(&mut self, ext: u8, m: M, imm: i32) {
        self.rex_m(true, ext, m);
        self.b(0x81);
        self.modrm_m(ext, m);
        self.d32(imm);
    }

    /// `add` (0x01), `cmp` (0x39) or `test` (0x85) `rm, reg`.
    fn alu_rr(&mut self, opcode: u8, rm: u8, reg: u8) {
        self.b(0x48 | (reg >> 3) << 2 | (rm >> 3));
        self.b(opcode);
        self.b(0xC0 | (reg & 7) << 3 | (rm & 7));
    }

    /// `imul dst, src`
    fn imul_rr(&mut self, dst: u8, src: u8) {
        self.b(0x48 | (dst >> 3) << 2 | (src >> 3));
        self.b(0x0F);
        self.b(0xAF);
        self.b(0xC0 | (dst & 7) << 3 | (src & 7));
    }

    /// Writes virtual-stack items to memory above the stack top, in order.
    /// Leaves the flags alone, so that a pending `Cond` stays valid.
    fn materialize(&mut self, items: &[VItem]) {
        for item in items {
            match *item {
                VItem::Reg(r) => {
                    self.store_imm(top(0), TAG_INT as i32);
                    self.store(top(0).at(8), r);
                }
                VItem::Imm(n) => {
                    self.store_imm(top(0), TAG_INT as i32);
                    match i32::try_from(n) {
                        Ok(n) => self.store_imm(top(0).at(8), n),
                        Err(_) => {
                            self.mov_imm64(RAX, n);
                            self.store(top(0).at(8), RAX);
                        }
                    }
                }
                VItem::Cond(cc) => {
                    self.store_imm(top(0), TAG_BOOL as i32);
                    self.setcc(cc, top(0).at(1));
                }
            }
            self.lea(R13, top(0).at(V));
        }
    }

    /// `inc qword [m]`
    fn inc_mem(&mut self, m: M) {
        self.rex_m(true, 0, m);
        self.b(0xFF);
        self.modrm_m(0, m);
    }

    /// `dec qword [m]`
    fn dec_mem(&mut self, m: M) {
        self.rex_m(true, 1, m);
        self.b(0xFF);
        self.modrm_m(1, m);
    }

    /// Copies the value at `src` to `dst`, using rax. With `share`, a heap
    /// value gains an owner: the copy is a clone. Without it, the value moves.
    fn copy_value(&mut self, src: M, dst: M, share: bool) {
        for offset in [0, 8, 16] {
            self.load(RAX, src.at(offset));
            self.store(dst.at(offset), RAX);
        }
        if share {
            let done = self.label();
            self.cmp_byte(dst, FIRST_HEAP_TAG);
            self.jcc(CC_B, done);
            self.load(RAX, dst.at(8));
            self.inc_mem(M(RAX, 0));
            self.bind(done);
        }
    }

    /// Leaves in rcx the address of the cache entry for the arguments in the
    /// first `n` slots. Uses rax.
    fn memo_entry(&mut self, n: usize, table: usize) {
        self.load(RAX, slot(0).at(8));
        for i in 1..n as u32 {
            self.imul_imm(RAX, 1_000_003);
            self.alu_m(0x03, RAX, slot(i).at(8));
        }
        self.alu_imm(4, RAX, MEMO_ENTRIES as i32 - 1); // and
        self.imul_imm(RAX, (n as i32 + 2) * 8);
        self.mov_imm64(RCX, table as i64);
        self.alu_rr(0x01, RCX, RAX);
    }

    /// Records the result in rdx for the arguments in the first `n` slots.
    /// Parameters are never assigned to, so the slots still hold the arguments.
    fn memo_store(&mut self, n: usize, table: usize) {
        self.memo_entry(n, table);
        self.store_imm(M(RCX, 0), 1);
        for i in 0..n as u32 {
            self.load(RAX, slot(i).at(8));
            self.store(M(RCX, 8 + 8 * i as i32), RAX);
        }
        self.store(M(RCX, 8 + 8 * n as i32), RDX);
    }

    /// `mov qword [m], value`, using `scratch` when the value needs 64 bits.
    fn store_value(&mut self, m: M, value: usize, scratch: u8) {
        match i32::try_from(value) {
            Ok(v) => self.store_imm(m, v),
            Err(_) => {
                self.mov_imm64(scratch, value as i64);
                self.store(m, scratch);
            }
        }
    }

    /// `setcc byte [m]`
    fn setcc(&mut self, cc: u8, m: M) {
        self.rex_m(false, 0, m);
        self.b(0x0F);
        self.b(0x90 | cc);
        self.modrm_m(0, m);
    }

    fn jmp(&mut self, label: usize) {
        self.b(0xE9);
        self.rel32(label);
    }

    fn jcc(&mut self, cc: u8, label: usize) {
        self.b(0x0F);
        self.b(0x80 | cc);
        self.rel32(label);
    }

    fn call(&mut self, address: usize) {
        self.mov_imm64(RAX, address as i64);
        self.buf.extend_from_slice(&[0xFF, 0xD0]);
    }

    /// Makes the stack top visible to the interpreter.
    fn sync(&mut self) {
        self.store(M(RBX, STACK_TOP_OFFSET), R13);
        self.store(M(RBX, FRAMES_TOP_OFFSET), R14);
    }

    /// Takes over both stack tops after the interpreter changed them.
    fn reload_all(&mut self) {
        self.load(R13, M(RBX, STACK_TOP_OFFSET));
        self.load(R14, M(RBX, FRAMES_TOP_OFFSET));
    }

    fn reload(&mut self) {
        self.load(R13, M(RBX, STACK_TOP_OFFSET));
    }

    /// Integer `add`, `sub` or `mul` of rax with a memory operand.
    fn int_op_m(&mut self, op: BinOp, m: M) {
        match op {
            BinOp::Add => self.alu_m(0x03, RAX, m),
            BinOp::Sub => self.alu_m(0x2B, RAX, m),
            _ => self.imul_m(RAX, m),
        }
    }

    /// Divides rax by rcx, leaving the quotient or the remainder in rax.
    /// The caller has ruled out the divisors 0 and -1.
    fn divide(&mut self, op: BinOp) {
        self.buf.extend_from_slice(&[0x48, 0x99]); // cqo
        self.buf.extend_from_slice(&[0x48, 0xF7, 0xF9]); // idiv rcx
        if op == BinOp::Rem {
            self.mov_rr(RAX, RDX);
        }
    }

    /// Divides rax by the constant `n` (not 0 or -1), leaving the quotient or
    /// the remainder in rax. For `n >= 2` the division is done by multiplying
    /// with a precomputed reciprocal, which is several times faster than `idiv`.
    fn divide_by(&mut self, op: BinOp, n: i32) {
        if n == 1 {
            if op == BinOp::Rem {
                self.mov_imm64(RAX, 0);
            }
            return;
        }
        if n < 2 {
            self.mov_imm64(RCX, n as i64);
            self.divide(op);
            return;
        }
        let (multiplier, shift) = reciprocal(n as i64);
        self.mov_rr(RCX, RAX);
        self.mov_imm64(RDX, multiplier);
        self.buf.extend_from_slice(&[0x48, 0xF7, 0xEA]); // imul rdx: rdx:rax = rax * rdx
        if multiplier < 0 {
            self.alu_rr(0x01, RDX, RCX);
        }
        if shift > 0 {
            self.buf.extend_from_slice(&[0x48, 0xC1, 0xFA, shift as u8]); // sar rdx, shift
        }
        // Round toward zero: add one when the quotient is negative.
        self.mov_rr(RAX, RDX);
        self.buf.extend_from_slice(&[0x48, 0xC1, 0xE8, 63]); // shr rax, 63
        self.alu_rr(0x01, RAX, RDX);
        if op == BinOp::Rem {
            // remainder = dividend - quotient * n
            self.imul_imm(RAX, n);
            self.alu_rr(0x29, RCX, RAX);
            self.mov_rr(RAX, RCX);
        }
    }

    fn int_op_imm(&mut self, op: BinOp, imm: i32) {
        match op {
            BinOp::Add => self.alu_imm(0, RAX, imm),
            BinOp::Sub => self.alu_imm(5, RAX, imm),
            _ => self.imul_imm(RAX, imm),
        }
    }
}

/// The multiplier and shift that turn a signed 64-bit division by `d >= 2`
/// into a multiplication: with `hi` the upper half of `n * multiplier`,
/// `n / d = ((hi + n if multiplier < 0) >> shift) + (1 if that is negative)`.
/// (Hacker's Delight, chapter 10.)
fn reciprocal(d: i64) -> (i64, u32) {
    const TWO_63: u64 = 1 << 63;
    let ad = d as u64;
    let anc = TWO_63 - 1 - (TWO_63 % ad);
    let mut p = 63;
    let mut q1 = TWO_63 / anc;
    let mut r1 = TWO_63 - q1 * anc;
    let mut q2 = TWO_63 / ad;
    let mut r2 = TWO_63 - q2 * ad;
    loop {
        p += 1;
        q1 = q1.wrapping_mul(2);
        r1 = r1.wrapping_mul(2);
        if r1 >= anc {
            q1 = q1.wrapping_add(1);
            r1 -= anc;
        }
        q2 = q2.wrapping_mul(2);
        r2 = r2.wrapping_mul(2);
        if r2 >= ad {
            q2 = q2.wrapping_add(1);
            r2 -= ad;
        }
        let delta = ad - r2;
        if !(q1 < delta || (q1 == delta && r1 == 0)) {
            break;
        }
    }
    (q2.wrapping_add(1) as i64, p - 64)
}

#[cfg(test)]
mod tests {
    use super::reciprocal;

    /// The steps `Asm::divide_by` emits, in plain Rust.
    fn divide(n: i64, d: i64) -> i64 {
        let (multiplier, shift) = reciprocal(d);
        let mut hi = ((n as i128 * multiplier as i128) >> 64) as i64;
        if multiplier < 0 {
            hi = hi.wrapping_add(n);
        }
        hi >>= shift;
        hi + ((hi as u64) >> 63) as i64
    }

    #[test]
    fn reciprocal_division_matches_idiv() {
        let divisors = [2, 3, 4, 5, 7, 8, 10, 16, 24, 100, 641, 1000, 65536, 1_000_000_007, i32::MAX as i64];
        let mut values: Vec<i64> = (-2000..=2000).collect();
        for edge in [i64::MAX, i64::MIN, i64::MAX / 2, i64::MIN / 2, 1 << 40, -(1 << 40), 123_456_789_012_345] {
            for delta in -3..=3i64 {
                values.push(edge.wrapping_add(delta));
            }
        }
        // A cheap pseudo-random sweep over the whole range.
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        for _ in 0..20_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            values.push(x as i64);
        }
        for d in divisors {
            for &n in &values {
                assert_eq!(divide(n, d), n / d, "{n} / {d}");
            }
        }
    }
}

/// Out-of-line code that asks the interpreter to do what a fast path cannot.
struct Stub {
    entry: usize,
    resume: usize,
    kind: StubKind,
}

enum StubKind {
    /// Run the op at `pc`.
    Op { pc: u32, target: Option<usize> },
    Call { index: u32, argc: u32, pc: u32 },
    CallValue { argc: u32, pc: u32 },
    Return,
    /// Write these values to memory, then finish the call in the interpreter.
    ReturnValue { items: Vec<VItem> },
    /// An unchecked `Int` op overflowed: put its operands back in memory and
    /// let the interpreter run the op, which reports the error.
    Fail { pc: u32, items: Vec<VItem> },
}

struct Gen<'a> {
    a: Asm,
    helpers: &'a Helpers,
    func: &'a Func,
    callee: &'a dyn Fn(u32) -> Option<Callee>,
    /// Values computed but not yet written to the value stack.
    vstack: Vec<VItem>,
    /// Registers from `TEMPS` that hold nothing.
    free: Vec<u8>,
    /// The register each local slot lives in, if any.
    slot_reg: Vec<Option<u8>>,
    /// One label per op, plus one for the end of the code.
    ops: Vec<usize>,
    stubs: Vec<Stub>,
    /// The first instruction of the function body.
    entry: usize,
    /// Distance from the start of a function's code to its body. The same
    /// for every function.
    body_offset: i32,
    exit: usize,
    fail: usize,
    status: usize,
}

impl Gen<'_> {
    /// Calls the interpreter to run the op at `pc`, then acts on its answer.
    fn run_op(&mut self, pc: u32, target: Option<usize>) {
        self.a.sync();
        self.a.mov_rr(RDI, RBX);
        self.a.mov_imm32(RSI, pc);
        self.a.call(self.helpers.op);
        self.a.reload();
        match target {
            Some(target) => {
                self.a.buf.extend_from_slice(&[0x83, 0xF8, JUMP as u8]); // cmp eax, JUMP
                self.a.jcc(CC_E, target);
                self.a.jcc(CC_A, self.status);
            }
            None => {
                self.a.buf.extend_from_slice(&[0x85, 0xC0]); // test eax, eax
                self.a.jcc(CC_NE, self.status);
            }
        }
    }

    /// Reserves a stub for the op at `pc` and returns the label of its entry.
    /// The fast path must be followed by `resume`.
    fn stub(&mut self, pc: u32, target: Option<usize>) -> (usize, usize) {
        self.stub_of(StubKind::Op { pc, target })
    }

    fn stub_of(&mut self, kind: StubKind) -> (usize, usize) {
        let entry = self.a.label();
        let resume = self.a.label();
        self.stubs.push(Stub { entry, resume, kind });
        (entry, resume)
    }

    /// Asks the interpreter to start a call, then enters the callee.
    fn call_helper(&mut self, index: u32, argc: u32, pc: u32) {
        let a = &mut self.a;
        a.sync();
        a.mov_rr(RDI, RBX);
        a.mov_imm32(RSI, index);
        a.mov_imm32(RDX, argc);
        a.mov_imm32(RCX, pc);
        a.call(self.helpers.call);
        self.enter_callee();
    }

    /// Asks the interpreter to start a call to the function value on top of
    /// the stack, then enters the callee.
    fn call_value_helper(&mut self, argc: u32, pc: u32) {
        let a = &mut self.a;
        a.sync();
        a.mov_rr(RDI, RBX);
        a.mov_imm32(RSI, argc);
        a.mov_imm32(RDX, pc);
        a.call(self.helpers.call_value);
        self.enter_callee();
    }

    /// A call to the function value on top of the stack, without leaving
    /// machine code when the callee is already compiled and both stacks have
    /// room. A closure's captured values become the locals after the arguments.
    fn call_value(&mut self, argc: u32, pc: u32) {
        let span = self.func.spans[pc as usize];
        let (slow, resume) = self.stub_of(StubKind::CallValue { argc, pc });
        let fail = self.fail;
        let closure = RC_DATA + self.helpers.closure_func;
        let captured = RC_DATA + self.helpers.closure_captured;
        let (vec_ptr, vec_len) = (self.helpers.vec_ptr, self.helpers.vec_len);
        let func_native = std::mem::offset_of!(Func, native) as i32;
        let func_need = std::mem::offset_of!(Func, need) as i32;
        let func_nlocals = std::mem::offset_of!(Func, nlocals) as i32;
        let body_offset = self.body_offset;
        let a = &mut self.a;
        let (is_fn, common) = (a.label(), a.label());
        let (copy, copied, fill, filled) = (a.label(), a.label(), a.label(), a.label());

        // rax = the callee's reference counts, rsi = its `Func`,
        // r8 / r9 = address and number of the captured values.
        // The callee value is dropped below, which must not free it.
        a.load(RAX, top(1).at(8));
        a.cmp_byte(top(1), TAG_FN);
        a.jcc(CC_E, is_fn);
        a.cmp_byte(top(1), TAG_CLOSURE);
        a.jcc(CC_NE, slow);
        a.cmp_imm(M(RAX, 0), 1);
        a.jcc(CC_E, slow);
        a.load(RSI, M(RAX, closure));
        a.load(R8, M(RAX, captured + vec_ptr));
        a.load(R9, M(RAX, captured + vec_len));
        a.jmp(common);
        a.bind(is_fn);
        a.cmp_imm(M(RAX, 0), 1);
        a.jcc(CC_E, slow);
        a.mov_rr(RSI, RAX);
        a.mov_imm64(R9, 0);
        a.bind(common);
        a.alu_imm(0, RSI, RC_DATA);

        // rdi = the callee's machine code, rcx = the new frame.
        a.load(RDI, M(RSI, func_native));
        a.alu_rr(0x85, RDI, RDI);
        a.jcc(CC_E, slow);
        a.alu_m(0x3B, R14, M(RBX, FRAMES_LIMIT_OFFSET));
        a.jcc(CC_AE, slow);
        a.load(RDX, M(RSI, func_need));
        a.imul_imm(RDX, V);
        a.alu_rr(0x01, RDX, R13);
        a.alu_m(0x3B, RDX, M(RBX, STACK_END_OFFSET));
        a.jcc(CC_A, slow);

        // From here on the call happens. Drop the callee value; rdx = first local.
        a.dec_mem(M(RAX, 0));
        a.alu_imm(5, R13, V);
        a.lea(RDX, top(argc as i32));
        a.store(M(R14, FRAME_BASE), RDX);
        a.bind(copy);
        a.alu_rr(0x85, R9, R9);
        a.jcc(CC_E, copied);
        a.copy_value(M(R8, 0), top(0), true);
        a.alu_imm(0, R8, V);
        a.alu_imm(0, R13, V);
        a.alu_imm(5, R9, 1);
        a.jmp(copy);
        a.bind(copied);
        // The remaining locals start out as `()`.
        a.load(RAX, M(RSI, func_nlocals));
        a.imul_imm(RAX, V);
        a.alu_rr(0x01, RAX, RDX);
        a.bind(fill);
        a.alu_rr(0x39, R13, RAX);
        a.jcc(CC_AE, filled);
        a.store_imm(top(0), TAG_UNIT as i32);
        a.alu_imm(0, R13, V);
        a.jmp(fill);
        a.bind(filled);

        a.store(M(R14, FRAME_FUNC), RSI);
        a.store_value(M(R14, FRAME_SPAN), span.start, RDX);
        a.store_value(M(R14, FRAME_SPAN + 8), span.end, RDX);
        a.alu_imm(0, R14, FRAME_SIZE);

        a.lea(RAX, M(RDI, body_offset));
        a.buf.extend_from_slice(&[0xFF, 0xD0]); // call rax
        a.buf.extend_from_slice(&[0x85, 0xC0]); // test eax, eax
        a.jcc(CC_NE, fail);
        a.bind(resume);
    }

    /// Leaves rdx pointing at item rcx of the list whose reference counts
    /// rdx points at. The caller has checked that the item exists.
    fn list_item(&mut self) {
        let a = &mut self.a;
        a.load(RDX, M(RDX, RC_DATA + self.helpers.vec_ptr));
        a.imul_imm(RCX, V);
        a.alu_rr(0x01, RDX, RCX);
    }

    /// Asks the interpreter to finish the current call, then leaves.
    fn return_helper(&mut self) {
        let a = &mut self.a;
        a.sync();
        a.mov_rr(RDI, RBX);
        a.call(self.helpers.ret);
        a.reload_all();
        if self.func.int_ret {
            a.load(RDX, top(1).at(8));
        }
        a.buf.extend_from_slice(&[0x31, 0xC0]); // xor eax, eax
        a.jmp(self.exit);
    }

    /// A call to a known function, without leaving machine code when the
    /// callee is already compiled and both stacks have room.
    fn call(&mut self, index: u32, argc: u32, pc: u32) {
        let callee = (self.callee)(index).filter(|c| {
            c.nlocals >= argc as usize
                && c.nlocals - argc as usize <= MAX_INLINE_LOCALS
                && c.need <= (i32::MAX / V) as usize
        });
        let Some(callee) = callee else {
            self.call_helper(index, argc, pc);
            return;
        };
        let span = self.func.spans[pc as usize];
        let extra = (callee.nlocals - argc as usize) as i32;
        let (slow, resume) = self.stub_of(StubKind::Call { index, argc, pc });
        let fail = self.fail;
        let a = &mut self.a;

        // A function that calls itself is certainly compiled, and its
        // address is known. Otherwise: rax = the callee's machine code, if any.
        let recursive = callee.func == self.func as *const Func as usize;
        let entry = self.entry;
        let body_offset = self.body_offset;
        if !recursive {
            a.mov_imm64(RAX, callee.native as i64);
            a.load(RAX, M(RAX, 0));
            a.buf.extend_from_slice(&[0x48, 0x85, 0xC0]); // test rax, rax
            a.jcc(CC_E, slow);
        }
        // r14 is the new frame; the interpreter reports running out of room.
        a.alu_m(0x3B, R14, M(RBX, FRAMES_LIMIT_OFFSET));
        a.jcc(CC_AE, slow);
        a.lea(RDX, top(0).at(callee.need as i32 * V));
        a.alu_m(0x3B, RDX, M(RBX, STACK_END_OFFSET));
        a.jcc(CC_A, slow);

        // The arguments are the first locals; the rest start out as `()`.
        a.lea(RDX, top(argc as i32));
        a.store(M(R14, FRAME_BASE), RDX);
        for i in 0..extra {
            a.store_imm(top(0).at(i * V), TAG_UNIT as i32);
        }
        if extra > 0 {
            a.alu_imm(0, R13, extra * V);
        }
        a.store_value(M(R14, FRAME_FUNC), callee.func, RDX);
        a.store_value(M(R14, FRAME_SPAN), span.start, RDX);
        a.store_value(M(R14, FRAME_SPAN + 8), span.end, RDX);
        a.alu_imm(0, R14, FRAME_SIZE);

        // Enter the body. It returns with its result on top of the stack
        // (r13) and its frame gone (r14).
        if recursive {
            a.b(0xE8); // call rel32
            a.rel32(entry);
        } else {
            a.alu_imm(0, RAX, body_offset);
            a.buf.extend_from_slice(&[0xFF, 0xD0]); // call rax
        }
        a.buf.extend_from_slice(&[0x85, 0xC0]); // test eax, eax
        a.jcc(CC_NE, fail);
        a.bind(resume);
    }

    /// Returns the single `Int` on the virtual stack by writing it straight
    /// into the result position, without first storing it on the stack.
    fn ret_value(&mut self, item: VItem) {
        let nlocals = self.func.nlocals;
        let (slow, _) = self.stub_of(StubKind::ReturnValue { items: vec![item] });
        let exit = self.exit;
        let checked: Vec<u32> = (0..nlocals as u32).filter(|s| !self.int_slot(*s)).collect();
        let a = &mut self.a;
        // The stack must hold exactly the locals, none of which needs dropping.
        a.lea(RAX, slot(nlocals as u32));
        a.buf.extend_from_slice(&[0x4C, 0x39, 0xE8]); // cmp rax, r13
        a.jcc(CC_NE, slow);
        for s in checked {
            a.cmp_byte(slot(s), FIRST_HEAP_TAG);
            a.jcc(CC_AE, slow);
        }
        // The result goes to the stack and, for callers that know it is an
        // `Int`, to rdx.
        a.store_imm(slot(0), TAG_INT as i32);
        match item {
            VItem::Reg(r) => a.mov_rr(RDX, r),
            VItem::Imm(n) => a.mov_imm64(RDX, n),
            VItem::Cond(_) => {}
        }
        if self.func.memo.get() != 0 {
            a.memo_store(self.func.nparams, self.func.memo.get());
        }
        a.store(slot(0).at(8), RDX);
        a.lea(R13, slot(1));
        a.alu_imm(5, R14, FRAME_SIZE);
        a.buf.extend_from_slice(&[0x31, 0xC0]); // xor eax, eax
        a.jmp(exit);
    }

    /// A return, without leaving machine code when no local needs dropping.
    fn ret(&mut self) {
        let nlocals = self.func.nlocals;
        if nlocals > MAX_INLINE_LOCALS {
            self.return_helper();
            return;
        }
        let (slow, _) = self.stub_of(StubKind::Return);
        let exit = self.exit;
        let unit_ret = self.func.unit_ret;
        let a = &mut self.a;

        // The stack must hold exactly the locals and the result.
        a.lea(RAX, slot(nlocals as u32 + 1));
        a.buf.extend_from_slice(&[0x4C, 0x39, 0xE8]); // cmp rax, r13
        a.jcc(CC_NE, slow);
        for i in 0..nlocals as u32 {
            // An `Int` slot never holds anything that needs dropping.
            if self.func.int_slots.get(i as usize).copied().unwrap_or(false) {
                continue;
            }
            a.cmp_byte(slot(i), FIRST_HEAP_TAG);
            a.jcc(CC_AE, slow);
        }
        if self.func.memo.get() != 0 {
            a.load(RDX, top(1).at(8));
            a.memo_store(self.func.nparams, self.func.memo.get());
        }
        if unit_ret {
            // The result is discarded, so it must not need dropping either.
            a.cmp_byte(top(1), FIRST_HEAP_TAG);
            a.jcc(CC_AE, slow);
            a.store_imm(slot(0), TAG_UNIT as i32);
        } else if nlocals > 0 {
            for offset in [0, 8, 16] {
                a.load(RAX, top(1).at(offset));
                a.store(slot(0).at(offset), RAX);
            }
        }
        if self.func.int_ret {
            a.load(RDX, slot(0).at(8));
        }
        // The result replaces the locals, and the frame is gone.
        a.lea(R13, slot(1));
        a.alu_imm(5, R14, FRAME_SIZE);
        a.buf.extend_from_slice(&[0x31, 0xC0]); // xor eax, eax
        a.jmp(exit);
    }

    /// Jumps to `slow` unless the value at `m` is an Int.
    fn expect_int(&mut self, m: M, slow: usize) {
        self.a.cmp_byte(m, TAG_INT);
        self.a.jcc(CC_NE, slow);
    }

    /// Completes a call: `rax` holds the callee's entry address, or 0 on failure.
    fn enter_callee(&mut self) {
        self.a.buf.extend_from_slice(&[0x48, 0x85, 0xC0]); // test rax, rax
        self.a.jcc(CC_E, self.fail);
        self.a.mov_rr(RDI, RBX);
        self.a.buf.extend_from_slice(&[0xFF, 0xD0]); // call rax
        self.a.buf.extend_from_slice(&[0x85, 0xC0]); // test eax, eax
        self.a.jcc(CC_NE, self.fail);
        self.a.reload_all();
    }

    // ---- typed code: `Int` values in registers ----

    fn int_slot(&self, s: u32) -> bool {
        self.func.int_slots.get(s as usize).copied().unwrap_or(false)
    }

    fn reg_of(&self, s: u32) -> Option<u8> {
        self.slot_reg.get(s as usize).copied().flatten()
    }

    /// Writes the virtual stack to memory.
    fn flush(&mut self) {
        let items = std::mem::take(&mut self.vstack);
        self.a.materialize(&items);
        for item in items {
            if let VItem::Reg(r) = item {
                self.free.push(r);
            }
        }
    }

    /// A free register for the virtual stack.
    fn tmp(&mut self) -> u8 {
        if self.free.is_empty() {
            self.flush();
        }
        self.free.pop().expect("a virtual-stack register is free after a flush")
    }

    /// Pops the top value, known to be an `Int`, into a register the caller owns.
    fn pop_int(&mut self) -> u8 {
        match self.vstack.pop() {
            Some(VItem::Reg(r)) => r,
            Some(VItem::Imm(n)) => {
                let r = self.tmp();
                self.a.mov_imm64(r, n);
                r
            }
            other => {
                // In memory (a `Cond` is never an `Int`, and is flushed before any op).
                if let Some(item) = other {
                    self.vstack.push(item);
                    self.flush();
                }
                let r = self.tmp();
                self.a.load(r, top(1).at(8));
                self.a.alu_imm(5, R13, V);
                r
            }
        }
    }

    /// Stores the registers that hold variables back into their slots.
    fn write_back(&mut self) {
        for (s, reg) in self.slot_reg.clone().into_iter().enumerate() {
            if let Some(reg) = reg {
                self.a.store_imm(slot(s as u32), TAG_INT as i32);
                self.a.store(slot(s as u32).at(8), reg);
            }
        }
    }

    /// Loads the value of an `Int` slot into rax.
    fn slot_to_rax(&mut self, s: u32) {
        match self.reg_of(s) {
            Some(r) => self.a.mov_rr(RAX, r),
            None => self.a.load(RAX, slot(s).at(8)),
        }
    }

    /// `cmp slot, imm` for an `Int` slot.
    fn cmp_slot_imm(&mut self, s: u32, n: i32) {
        match self.reg_of(s) {
            Some(r) => self.a.alu_imm(7, r, n),
            None => self.a.cmp_imm(slot(s).at(8), n),
        }
    }

    /// `cmp rax, slot` for an `Int` slot.
    fn cmp_rax_slot(&mut self, s: u32) {
        match self.reg_of(s) {
            Some(r) => self.a.alu_rr(0x39, RAX, r),
            None => self.a.alu_m(0x3B, RAX, slot(s).at(8)),
        }
    }

    /// The label of code that reports the failure of the op at `pc`, whose
    /// operands are `operands` on top of the current virtual stack.
    fn fail_stub(&mut self, pc: u32, operands: &[u8]) -> usize {
        let mut items = self.vstack.clone();
        items.extend(operands.iter().map(|r| VItem::Reg(*r)));
        self.stub_of(StubKind::Fail { pc, items }).0
    }

    /// Generates unchecked code for an op on `Int` values. Returns false when
    /// the op is not one of those, and must go through `op`.
    fn typed(&mut self, pc: u32, op: Op) -> bool {
        match op {
            Op::Int(n) => self.vstack.push(VItem::Imm(n)),
            Op::Load(s) if self.int_slot(s) => {
                let t = self.tmp();
                match self.reg_of(s) {
                    Some(r) => self.a.mov_rr(t, r),
                    None => self.a.load(t, slot(s).at(8)),
                }
                self.vstack.push(VItem::Reg(t));
            }
            Op::Store(s) if self.int_slot(s) => match (self.reg_of(s), self.vstack.pop()) {
                (Some(r), Some(VItem::Imm(n))) => self.a.mov_imm64(r, n),
                (reg, item) => {
                    if let Some(item) = item {
                        self.vstack.push(item);
                    }
                    let x = self.pop_int();
                    match reg {
                        Some(r) => self.a.mov_rr(r, x),
                        None => {
                            self.a.store_imm(slot(s), TAG_INT as i32);
                            self.a.store(slot(s).at(8), x);
                        }
                    }
                    self.free.push(x);
                }
            },
            Op::Pop if !self.vstack.is_empty() => {
                if let Some(VItem::Reg(r)) = self.vstack.pop() {
                    self.free.push(r);
                }
            }
            Op::ArithSlotInt(op @ (BinOp::Add | BinOp::Sub | BinOp::Mul), s, n) => {
                let t = self.tmp();
                let slow = self.fail_stub(pc, &[]);
                self.slot_to_rax(s);
                self.a.int_op_imm(op, n);
                self.a.jcc(CC_O, slow);
                self.a.mov_rr(t, RAX);
                self.vstack.push(VItem::Reg(t));
            }
            Op::ArithSlotInt(op @ (BinOp::Div | BinOp::Rem), s, n) if n != 0 && n != -1 => {
                let t = self.tmp();
                self.slot_to_rax(s);
                self.a.divide_by(op, n);
                self.a.mov_rr(t, RAX);
                self.vstack.push(VItem::Reg(t));
            }
            Op::ArithSlotInt(..) => {
                // Division by 0 or -1: the interpreter reads the slot from memory.
                self.flush();
                self.write_back();
                self.run_op(pc, None);
            }
            Op::ArithInt(op @ (BinOp::Add | BinOp::Sub | BinOp::Mul), n) => {
                let x = self.pop_int();
                let slow = self.fail_stub(pc, &[x]);
                self.a.mov_rr(RAX, x);
                self.a.int_op_imm(op, n);
                self.a.jcc(CC_O, slow);
                self.a.mov_rr(x, RAX);
                self.vstack.push(VItem::Reg(x));
            }
            Op::ArithInt(op @ (BinOp::Div | BinOp::Rem), n) if n != 0 && n != -1 => {
                let x = self.pop_int();
                self.a.mov_rr(RAX, x);
                self.a.divide_by(op, n);
                self.a.mov_rr(x, RAX);
                self.vstack.push(VItem::Reg(x));
            }
            Op::Arith(op @ (BinOp::Add | BinOp::Sub | BinOp::Mul)) if self.func.int_ops[pc as usize] => {
                let y = self.pop_int();
                let x = self.pop_int();
                let slow = self.fail_stub(pc, &[x, y]);
                self.a.mov_rr(RAX, x);
                match op {
                    BinOp::Add => self.a.alu_rr(0x01, RAX, y),
                    BinOp::Sub => self.a.alu_rr(0x29, RAX, y),
                    _ => self.a.imul_rr(RAX, y),
                }
                self.a.jcc(CC_O, slow);
                self.a.mov_rr(x, RAX);
                self.free.push(y);
                self.vstack.push(VItem::Reg(x));
            }
            Op::CmpSlotInt(op, s, n) => {
                self.cmp_slot_imm(s, n);
                self.vstack.push(VItem::Cond(condition(op)));
            }
            Op::CmpInt(op, n) => {
                let x = self.pop_int();
                self.a.alu_imm(7, x, n);
                self.free.push(x);
                self.vstack.push(VItem::Cond(condition(op)));
            }
            Op::Cmp(op) if self.func.int_ops[pc as usize] => {
                let y = self.pop_int();
                let x = self.pop_int();
                self.a.alu_rr(0x39, x, y);
                self.free.push(x);
                self.free.push(y);
                self.vstack.push(VItem::Cond(condition(op)));
            }
            Op::JumpIfFalse(target) if matches!(self.vstack.last(), Some(VItem::Cond(_))) => {
                let Some(VItem::Cond(cc)) = self.vstack.pop() else { return false };
                let target = self.ops[target as usize];
                self.flush();
                self.a.jcc(cc ^ 1, target);
            }
            Op::JumpUnlessSlotInt(op, s, n, target) => {
                let target = self.ops[target as usize];
                self.flush();
                self.cmp_slot_imm(s, n);
                self.a.jcc(condition(op) ^ 1, target);
            }
            Op::JumpUnlessInt(op, n, target) => {
                let target = self.ops[target as usize];
                let x = self.pop_int();
                self.flush();
                self.a.alu_imm(7, x, n);
                self.free.push(x);
                self.a.jcc(condition(op) ^ 1, target);
            }
            Op::Jump(target) => {
                // A jump to a return is a return: no need to pass the value through memory.
                if matches!(self.func.code[target as usize], Op::Return) && self.try_ret_value() {
                    return true;
                }
                let target = self.ops[target as usize];
                self.flush();
                self.a.jmp(target);
            }
            Op::Call(index, argc) if (self.callee)(index).is_some_and(|c| c.int_ret) => {
                // The callee leaves its `Int` result on the stack and in rdx.
                self.flush();
                self.call(index, argc, pc);
                self.a.alu_imm(5, R13, V);
                let t = self.tmp();
                self.a.mov_rr(t, RDX);
                self.vstack.push(VItem::Reg(t));
            }
            Op::ForRange(s, target) => {
                let target = self.ops[target as usize];
                self.flush();
                self.slot_to_rax(s);
                self.cmp_rax_slot(s + 1);
                self.a.jcc(CC_GE, target);
            }
            Op::ForRangeNext(s, target) => {
                let target = self.ops[target as usize];
                self.flush();
                match self.reg_of(s) {
                    Some(r) => {
                        self.a.alu_imm(0, r, 1);
                        self.a.mov_rr(RAX, r);
                    }
                    None => {
                        self.a.load(RAX, slot(s).at(8));
                        self.a.alu_imm(0, RAX, 1);
                        self.a.store(slot(s).at(8), RAX);
                    }
                }
                self.cmp_rax_slot(s + 1);
                self.a.jcc(CC_L, target);
            }
            _ => return false,
        }
        true
    }

    /// If the virtual stack holds exactly one `Int`, returns it from the
    /// function and reports true.
    fn try_ret_value(&mut self) -> bool {
        let [item @ (VItem::Reg(_) | VItem::Imm(_))] = self.vstack.as_slice() else { return false };
        if self.func.unit_ret || self.func.nlocals > MAX_INLINE_LOCALS {
            return false;
        }
        let item = *item;
        self.ret_value(item);
        self.vstack.clear();
        if let VItem::Reg(r) = item {
            self.free.push(r);
        }
        true
    }

    /// Generates code for one op.
    fn step(&mut self, pc: u32, op: Op) {
        // A pending condition is only usable by the jump that tests it.
        if matches!(self.vstack.last(), Some(VItem::Cond(_))) && !matches!(op, Op::JumpIfFalse(_)) {
            self.flush();
        }
        if matches!(op, Op::Return) && self.try_ret_value() {
            return;
        }
        if !self.typed(pc, op) {
            self.flush();
            self.op(pc, op);
        }
    }

    // ---- code that makes no assumption about types ----

    fn op(&mut self, pc: u32, op: Op) {
        let a = &mut self.a;
        match op {
            Op::Int(n) => {
                a.store_imm(top(0), TAG_INT as i32);
                match i32::try_from(n) {
                    Ok(n) => a.store_imm(top(0).at(8), n),
                    Err(_) => {
                        a.mov_imm64(RAX, n);
                        a.store(top(0).at(8), RAX);
                    }
                }
                a.alu_imm(0, R13, V);
            }
            Op::Bool(b) => {
                a.store_imm(top(0), TAG_BOOL as i32 | (b as i32) << 8);
                a.alu_imm(0, R13, V);
            }
            Op::Unit => {
                a.store_imm(top(0), TAG_UNIT as i32);
                a.alu_imm(0, R13, V);
            }
            Op::Load(s) => {
                a.copy_value(slot(s), top(0), true);
                a.alu_imm(0, R13, V);
            }
            Op::Take(s) => {
                a.copy_value(slot(s), top(0), false);
                a.store_imm(slot(s), TAG_UNIT as i32);
                a.alu_imm(0, R13, V);
            }
            Op::ForList(s, target) => {
                // List in `s`, position in `s + 1`, item goes to `s + 2`.
                let target = self.ops[target as usize];
                let vec_len = RC_DATA + self.helpers.vec_len;
                let (slow, resume) = self.stub(pc, Some(target));
                self.a.cmp_byte(slot(s), TAG_LIST);
                self.a.jcc(CC_NE, slow);
                self.expect_int(slot(s + 1), slow);
                // The previous item is overwritten, so it must not need dropping.
                self.a.cmp_byte(slot(s + 2), FIRST_HEAP_TAG);
                self.a.jcc(CC_AE, slow);
                self.a.load(RDX, slot(s).at(8));
                self.a.load(RCX, slot(s + 1).at(8));
                self.a.alu_m(0x3B, RCX, M(RDX, vec_len));
                self.a.jcc(CC_AE, target);
                self.list_item();
                let a = &mut self.a;
                a.copy_value(M(RDX, 0), slot(s + 2), true);
                a.inc_mem(slot(s + 1).at(8));
                a.bind(resume);
            }
            Op::Index => {
                let vec_len = RC_DATA + self.helpers.vec_len;
                let (slow, resume) = self.stub(pc, None);
                self.a.cmp_byte(top(2), TAG_LIST);
                self.a.jcc(CC_NE, slow);
                self.expect_int(top(1), slow);
                // The list on the stack is dropped, which must not free it.
                self.a.load(RDX, top(2).at(8));
                self.a.cmp_imm(M(RDX, 0), 1);
                self.a.jcc(CC_E, slow);
                self.a.load(RCX, top(1).at(8));
                self.a.alu_m(0x3B, RCX, M(RDX, vec_len));
                self.a.jcc(CC_AE, slow);
                self.a.dec_mem(M(RDX, 0));
                self.list_item();
                let a = &mut self.a;
                a.copy_value(M(RDX, 0), top(2), true);
                a.alu_imm(5, R13, V);
                a.bind(resume);
            }
            Op::InPlace(s, path, InPlace::Push) if path == NONE => {
                // Append in place when the list has one owner and spare room.
                let (vec_ptr, vec_len, vec_cap) = (
                    RC_DATA + self.helpers.vec_ptr,
                    RC_DATA + self.helpers.vec_len,
                    RC_DATA + self.helpers.vec_cap,
                );
                let (slow, resume) = self.stub(pc, None);
                let a = &mut self.a;
                a.cmp_byte(slot(s), TAG_LIST);
                a.jcc(CC_NE, slow);
                a.load(RDX, slot(s).at(8));
                a.cmp_imm(M(RDX, 0), 1);
                a.jcc(CC_NE, slow);
                a.cmp_imm(M(RDX, 8), 1);
                a.jcc(CC_NE, slow);
                a.load(RCX, M(RDX, vec_len));
                a.alu_m(0x3B, RCX, M(RDX, vec_cap));
                a.jcc(CC_AE, slow);
                a.mov_rr(RSI, RCX);
                a.imul_imm(RSI, V);
                a.alu_m(0x03, RSI, M(RDX, vec_ptr));
                a.copy_value(top(1), M(RSI, 0), false);
                a.alu_imm(0, RCX, 1);
                a.store(M(RDX, vec_len), RCX);
                a.alu_imm(5, R13, V);
                a.bind(resume);
            }
            Op::Store(s) => {
                // The old value must not need dropping; the new one is moved as is.
                let (slow, resume) = self.stub(pc, None);
                let a = &mut self.a;
                a.cmp_byte(slot(s), FIRST_HEAP_TAG);
                a.jcc(CC_AE, slow);
                for offset in [0, 8, 16] {
                    a.load(RAX, top(1).at(offset));
                    a.store(slot(s).at(offset), RAX);
                }
                a.alu_imm(5, R13, V);
                a.bind(resume);
            }
            Op::Pop => {
                let (slow, resume) = self.stub(pc, None);
                let a = &mut self.a;
                a.cmp_byte(top(1), FIRST_HEAP_TAG);
                a.jcc(CC_AE, slow);
                a.alu_imm(5, R13, V);
                a.bind(resume);
            }
            Op::Arith(op @ (BinOp::Add | BinOp::Sub | BinOp::Mul)) => {
                let (slow, resume) = self.stub(pc, None);
                self.expect_int(top(2), slow);
                self.expect_int(top(1), slow);
                let a = &mut self.a;
                a.load(RAX, top(2).at(8));
                a.int_op_m(op, top(1).at(8));
                a.jcc(CC_O, slow);
                a.store(top(2).at(8), RAX);
                a.alu_imm(5, R13, V);
                a.bind(resume);
            }
            Op::Arith(op @ (BinOp::Div | BinOp::Rem)) => {
                let (slow, resume) = self.stub(pc, None);
                self.expect_int(top(2), slow);
                self.expect_int(top(1), slow);
                let a = &mut self.a;
                // Division by 0 fails and by -1 can overflow: leave both to the interpreter.
                a.load(RCX, top(1).at(8));
                a.lea(RAX, M(RCX, 1));
                a.alu_imm(7, RAX, 1);
                a.jcc(CC_BE, slow);
                a.load(RAX, top(2).at(8));
                a.divide(op);
                a.store(top(2).at(8), RAX);
                a.alu_imm(5, R13, V);
                a.bind(resume);
            }
            Op::Cmp(op) => {
                let (slow, resume) = self.stub(pc, None);
                self.expect_int(top(2), slow);
                self.expect_int(top(1), slow);
                let a = &mut self.a;
                a.load(RAX, top(2).at(8));
                a.alu_m(0x3B, RAX, top(1).at(8));
                a.store_imm(top(2), TAG_BOOL as i32);
                a.setcc(condition(op), top(2).at(1));
                a.alu_imm(5, R13, V);
                a.bind(resume);
            }
            Op::ArithSlotInt(op @ (BinOp::Add | BinOp::Sub | BinOp::Mul), s, n) => {
                let (slow, resume) = self.stub(pc, None);
                self.expect_int(slot(s), slow);
                let a = &mut self.a;
                a.load(RAX, slot(s).at(8));
                a.int_op_imm(op, n);
                a.jcc(CC_O, slow);
                a.store_imm(top(0), TAG_INT as i32);
                a.store(top(0).at(8), RAX);
                a.alu_imm(0, R13, V);
                a.bind(resume);
            }
            Op::ArithSlotInt(op @ (BinOp::Div | BinOp::Rem), s, n) if n != 0 && n != -1 => {
                let (slow, resume) = self.stub(pc, None);
                self.expect_int(slot(s), slow);
                let a = &mut self.a;
                a.load(RAX, slot(s).at(8));
                a.divide_by(op, n);
                a.store_imm(top(0), TAG_INT as i32);
                a.store(top(0).at(8), RAX);
                a.alu_imm(0, R13, V);
                a.bind(resume);
            }
            Op::CmpSlotInt(op, s, n) => {
                let (slow, resume) = self.stub(pc, None);
                self.expect_int(slot(s), slow);
                let a = &mut self.a;
                a.cmp_imm(slot(s).at(8), n);
                a.store_imm(top(0), TAG_BOOL as i32);
                a.setcc(condition(op), top(0).at(1));
                a.alu_imm(0, R13, V);
                a.bind(resume);
            }
            Op::ArithInt(op @ (BinOp::Add | BinOp::Sub | BinOp::Mul), n) => {
                let (slow, resume) = self.stub(pc, None);
                self.expect_int(top(1), slow);
                let a = &mut self.a;
                a.load(RAX, top(1).at(8));
                a.int_op_imm(op, n);
                a.jcc(CC_O, slow);
                a.store(top(1).at(8), RAX);
                a.bind(resume);
            }
            Op::ArithInt(op @ (BinOp::Div | BinOp::Rem), n) if n != 0 && n != -1 => {
                let (slow, resume) = self.stub(pc, None);
                self.expect_int(top(1), slow);
                let a = &mut self.a;
                a.load(RAX, top(1).at(8));
                a.divide_by(op, n);
                a.store(top(1).at(8), RAX);
                a.bind(resume);
            }
            Op::CmpInt(op, n) => {
                let (slow, resume) = self.stub(pc, None);
                self.expect_int(top(1), slow);
                let a = &mut self.a;
                a.cmp_imm(top(1).at(8), n);
                a.store_imm(top(1), TAG_BOOL as i32);
                a.setcc(condition(op), top(1).at(1));
                a.bind(resume);
            }
            Op::JumpUnlessSlotInt(op, s, n, target) => {
                let target = self.ops[target as usize];
                let (slow, resume) = self.stub(pc, Some(target));
                self.expect_int(slot(s), slow);
                let a = &mut self.a;
                a.cmp_imm(slot(s).at(8), n);
                a.jcc(condition(op) ^ 1, target);
                a.bind(resume);
            }
            Op::JumpUnlessInt(op, n, target) => {
                let target = self.ops[target as usize];
                let (slow, resume) = self.stub(pc, Some(target));
                self.expect_int(top(1), slow);
                let a = &mut self.a;
                a.alu_imm(5, R13, V);
                a.cmp_imm(top(0).at(8), n);
                a.jcc(condition(op) ^ 1, target);
                a.bind(resume);
            }
            Op::Jump(target) => {
                let target = self.ops[target as usize];
                self.a.jmp(target);
            }
            Op::JumpIfFalse(target) => {
                let target = self.ops[target as usize];
                let (slow, resume) = self.stub(pc, Some(target));
                let a = &mut self.a;
                a.cmp_byte(top(1), TAG_BOOL);
                a.jcc(CC_NE, slow);
                a.alu_imm(5, R13, V);
                a.cmp_byte(top(0).at(1), 0);
                a.jcc(CC_E, target);
                a.bind(resume);
            }
            Op::ForRange(s, target) => {
                let target = self.ops[target as usize];
                let (slow, resume) = self.stub(pc, Some(target));
                self.expect_int(slot(s), slow);
                self.expect_int(slot(s + 1), slow);
                let a = &mut self.a;
                a.load(RAX, slot(s).at(8));
                a.alu_m(0x3B, RAX, slot(s + 1).at(8));
                a.jcc(CC_GE, target);
                a.bind(resume);
            }
            Op::ForRangeNext(s, target) => {
                let target = self.ops[target as usize];
                let (slow, resume) = self.stub(pc, Some(target));
                self.expect_int(slot(s), slow);
                self.expect_int(slot(s + 1), slow);
                let a = &mut self.a;
                a.load(RAX, slot(s).at(8));
                a.alu_imm(0, RAX, 1);
                a.store(slot(s).at(8), RAX);
                a.alu_m(0x3B, RAX, slot(s + 1).at(8));
                a.jcc(CC_L, target);
                a.bind(resume);
            }
            Op::Call(index, argc) => self.call(index, argc, pc),
            Op::CallValue(argc) => self.call_value(argc, pc),
            Op::Return => self.ret(),
            Op::AndJump(target) | Op::OrJump(target) => {
                let target = self.ops[target as usize];
                self.run_op(pc, Some(target));
            }
            Op::Try(target) if target != NONE => {
                let target = self.ops[target as usize];
                self.run_op(pc, Some(target));
            }
            _ => self.run_op(pc, None),
        }
    }
}

/// Compiles a function. The result is position independent.
/// `callee` describes the function with a given index, for calls to it.
pub fn compile(func: &Func, helpers: &Helpers, callee: &dyn Fn(u32) -> Option<Callee>) -> Vec<u8> {
    let mut a = Asm::default();
    let ops = (0..=func.code.len()).map(|_| a.label()).collect();
    let (exit, fail, status) = (a.label(), a.label(), a.label());
    let entry = a.label();
    let slot_reg = choose_slot_regs(func);
    let uses_slot_regs = slot_reg.iter().any(|r| r.is_some());
    let mut is_target = vec![false; func.code.len() + 1];
    for op in &func.code {
        if let Some(t) = target_of(*op) {
            is_target[t as usize] = true;
        }
    }
    let mut g = Gen {
        a,
        helpers,
        func,
        callee,
        vstack: Vec::new(),
        free: TEMPS.to_vec(),
        slot_reg,
        ops,
        stubs: Vec::new(),
        entry,
        body_offset: 0,
        exit,
        fail,
        status,
    };

    // Entry from the interpreter: save every register generated code uses,
    // load both stack tops, run the body, and store the tops back.
    // Seven pushes leave the native stack aligned for the call.
    let saved = [RBX, R12, R13, R14, R15, RBP, RAX];
    for reg in saved {
        g.a.push(reg);
    }
    g.a.mov_rr(RBX, RDI);
    g.a.reload_all();
    g.a.b(0xE8); // call rel32
    g.a.rel32(entry);
    // After a failure the interpreter's own record of the stacks stands.
    let failed = g.a.label();
    g.a.buf.extend_from_slice(&[0x85, 0xC0]); // test eax, eax
    g.a.jcc(CC_NE, failed);
    g.a.sync();
    g.a.bind(failed);
    g.a.pop(RCX);
    for reg in saved[..6].iter().rev() {
        g.a.pop(*reg);
    }
    g.a.b(0xC3);
    g.body_offset = g.a.buf.len() as i32;

    // The body, which generated code calls directly. rbx, r13 and r14 stay as
    // they are; it preserves the other registers it uses. One or three pushes
    // keep the native stack aligned.
    g.a.bind(entry);
    g.a.push(R12);
    if uses_slot_regs {
        for reg in SLOT_REGS {
            g.a.push(reg);
        }
    }
    g.a.lea(R12, M(R13, -(func.nlocals as i32) * V));
    // Arguments arrive in their slots; the other slots hold nothing yet.
    for (s, reg) in g.slot_reg.clone().into_iter().enumerate() {
        if let Some(reg) = reg {
            g.a.load(reg, slot(s as u32).at(8));
        }
    }

    // A memoized function first looks its arguments up in the result cache.
    // On a hit the call is over at once: the stack holds only the arguments
    // and empty locals, so the result simply replaces them.
    let memo = func.memo.get();
    if memo != 0 {
        let n = func.nparams;
        let miss = g.a.label();
        g.a.memo_entry(n, memo);
        g.a.cmp_imm(M(RCX, 0), 1);
        g.a.jcc(CC_NE, miss);
        for i in 0..n as u32 {
            g.a.load(RAX, slot(i).at(8));
            g.a.alu_m(0x3B, RAX, M(RCX, 8 + 8 * i as i32));
            g.a.jcc(CC_NE, miss);
        }
        g.a.load(RDX, M(RCX, 8 + 8 * n as i32));
        g.a.store_imm(slot(0), TAG_INT as i32);
        g.a.store(slot(0).at(8), RDX);
        g.a.lea(R13, slot(1));
        g.a.alu_imm(5, R14, FRAME_SIZE);
        g.a.buf.extend_from_slice(&[0x31, 0xC0]); // xor eax, eax
        g.a.jmp(exit);
        g.a.bind(miss);
    }

    for (pc, op) in func.code.iter().enumerate() {
        // Every way of reaching a jump target must agree on what is in memory.
        if is_target[pc] && !(matches!(op, Op::Return) && g.try_ret_value()) {
            g.flush();
        }
        g.a.bind(g.ops[pc]);
        g.step(pc as u32, *op);
    }
    g.flush();
    g.a.bind(g.ops[func.code.len()]);

    // Leave with the status in eax: 0 for success, 1 for a runtime error.
    g.a.bind(exit);
    if uses_slot_regs {
        for reg in SLOT_REGS.iter().rev() {
            g.a.pop(*reg);
        }
    }
    g.a.pop(R12);
    g.a.b(0xC3);
    g.a.bind(fail);
    g.a.mov_imm32(RAX, 1);
    g.a.jmp(exit);
    // A helper answered RETURNED or FAILED.
    g.a.bind(status);
    g.a.buf.extend_from_slice(&[0x83, 0xE8, RETURNED as u8]); // sub eax, RETURNED
    g.a.jcc(CC_NE, exit);
    // The interpreter finished the call: take over the stack tops it left.
    g.a.reload_all();
    if func.int_ret {
        g.a.load(RDX, top(1).at(8));
    }
    g.a.jmp(exit);

    for stub in std::mem::take(&mut g.stubs) {
        g.a.bind(stub.entry);
        match stub.kind {
            StubKind::Op { pc, target } => {
                g.run_op(pc, target);
                g.a.jmp(stub.resume);
            }
            StubKind::Call { index, argc, pc } => {
                g.call_helper(index, argc, pc);
                g.a.jmp(stub.resume);
            }
            StubKind::CallValue { argc, pc } => {
                g.call_value_helper(argc, pc);
                g.a.jmp(stub.resume);
            }
            StubKind::Return => g.return_helper(),
            StubKind::ReturnValue { items } => {
                g.a.materialize(&items);
                g.return_helper();
            }
            StubKind::Fail { pc, items } => {
                g.a.materialize(&items);
                g.write_back();
                g.run_op(pc, None);
                // The interpreter reports the error, so this is not reached.
                g.a.jmp(g.fail);
            }
        }
    }
    g.a.finish()
}

// ---- executable memory ----

#[cfg(windows)]
mod os {
    use std::ffi::c_void;

    #[link(name = "kernel32")]
    extern "system" {
        fn VirtualAlloc(address: *mut c_void, size: usize, kind: u32, protect: u32) -> *mut c_void;
        fn VirtualFree(address: *mut c_void, size: usize, kind: u32) -> i32;
    }

    pub fn alloc(size: usize) -> *mut u8 {
        const MEM_COMMIT_RESERVE: u32 = 0x3000;
        const PAGE_EXECUTE_READWRITE: u32 = 0x40;
        unsafe { VirtualAlloc(std::ptr::null_mut(), size, MEM_COMMIT_RESERVE, PAGE_EXECUTE_READWRITE) as *mut u8 }
    }

    pub fn free(address: *mut u8, _size: usize) {
        const MEM_RELEASE: u32 = 0x8000;
        unsafe {
            VirtualFree(address as *mut c_void, 0, MEM_RELEASE);
        }
    }
}

#[cfg(unix)]
mod os {
    use std::ffi::c_void;

    extern "C" {
        fn mmap(address: *mut c_void, size: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut c_void;
        fn munmap(address: *mut c_void, size: usize) -> i32;
    }

    #[cfg(target_os = "macos")]
    const MAP_ANON: i32 = 0x1000;
    #[cfg(not(target_os = "macos"))]
    const MAP_ANON: i32 = 0x20;

    pub fn alloc(size: usize) -> *mut u8 {
        const PROT_READ_WRITE_EXEC: i32 = 7;
        const MAP_PRIVATE: i32 = 2;
        let p = unsafe { mmap(std::ptr::null_mut(), size, PROT_READ_WRITE_EXEC, MAP_PRIVATE | MAP_ANON, -1, 0) };
        if p as isize == -1 {
            std::ptr::null_mut()
        } else {
            p as *mut u8
        }
    }

    pub fn free(address: *mut u8, size: usize) {
        unsafe {
            munmap(address as *mut c_void, size);
        }
    }
}

#[cfg(not(any(windows, unix)))]
mod os {
    pub fn alloc(_size: usize) -> *mut u8 {
        std::ptr::null_mut()
    }

    pub fn free(_address: *mut u8, _size: usize) {}
}

/// Executable memory holding the generated code of one program.
#[derive(Default)]
pub struct CodeArena {
    chunks: Vec<(*mut u8, usize)>,
    used: usize,
}

impl CodeArena {
    const CHUNK: usize = 1 << 20;

    /// Copies code into executable memory and returns its address, or None
    /// when the operating system refuses to provide such memory.
    pub fn install(&mut self, code: &[u8]) -> Option<usize> {
        let fits = self.chunks.last().is_some_and(|(_, size)| self.used + code.len() <= *size);
        if !fits {
            let size = code.len().max(Self::CHUNK);
            let chunk = os::alloc(size);
            if chunk.is_null() {
                return None;
            }
            self.chunks.push((chunk, size));
            self.used = 0;
        }
        let (chunk, _) = *self.chunks.last()?;
        // Safety: the chunk is writable and has room for `code` at `used`.
        let address = unsafe {
            let address = chunk.add(self.used);
            std::ptr::copy_nonoverlapping(code.as_ptr(), address, code.len());
            address
        };
        self.used += (code.len() + 15) & !15;
        Some(address as usize)
    }
}

impl Drop for CodeArena {
    fn drop(&mut self) {
        for (chunk, size) in self.chunks.drain(..) {
            os::free(chunk, size);
        }
    }
}
