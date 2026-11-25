//! x86-64 code generator: turns a function's bytecode into machine code.
//!
//! Integer arithmetic, comparisons, branches, counting loops and calls are
//! emitted as native instructions that work directly on the value stack.
//! Every fast path first checks that its operands are the plain values it
//! expects; when they are not, and for every other op, the code calls back into
//! the interpreter to run that single op. The two therefore always agree.
//!
//! Register use inside generated code:
//!   rbx  the machine's `Stack` (see `interp`)
//!   r12  address of the current call's first local slot
//!   r13  address just past the top of the value stack
//!
//! Generated functions and the helpers they call use the System V calling
//! convention on every operating system, so one code generator serves them all.

use crate::ast::BinOp;
use crate::bytecode::{Func, Op, NONE};

/// Size of a `Value` in bytes.
pub const VALUE_SIZE: usize = 24;
/// Discriminants of the plain values the generated code handles itself.
pub const TAG_INT: u8 = 0;
pub const TAG_BOOL: u8 = 2;
pub const TAG_UNIT: u8 = 3;
/// Values with this discriminant or above own heap memory.
pub const FIRST_HEAP_TAG: u8 = 4;
/// Offset of the top-of-stack pointer inside `Stack`.
pub const STACK_TOP_OFFSET: i32 = 8;

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
}

const V: i32 = VALUE_SIZE as i32;

const RAX: u8 = 0;
const RCX: u8 = 1;
const RDX: u8 = 2;
const RBX: u8 = 3;
const RSI: u8 = 6;
const RDI: u8 = 7;
const R12: u8 = 12;
const R13: u8 = 13;

// Condition codes.
const CC_O: u8 = 0x0;
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
        self.rex_m(true, 7, m);
        self.b(0x81);
        self.modrm_m(7, m);
        self.d32(imm);
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

    fn int_op_imm(&mut self, op: BinOp, imm: i32) {
        match op {
            BinOp::Add => self.alu_imm(0, RAX, imm),
            BinOp::Sub => self.alu_imm(5, RAX, imm),
            _ => self.imul_imm(RAX, imm),
        }
    }
}

/// Out-of-line code that runs one op in the interpreter when a fast path cannot.
struct Stub {
    entry: usize,
    resume: usize,
    pc: u32,
    target: Option<usize>,
}

struct Gen<'a> {
    a: Asm,
    helpers: &'a Helpers,
    /// One label per op, plus one for the end of the code.
    ops: Vec<usize>,
    stubs: Vec<Stub>,
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
        let entry = self.a.label();
        let resume = self.a.label();
        self.stubs.push(Stub { entry, resume, pc, target });
        (entry, resume)
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
        self.a.reload();
    }

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
                let (slow, resume) = self.stub(pc, None);
                let a = &mut self.a;
                a.cmp_byte(slot(s), FIRST_HEAP_TAG);
                a.jcc(CC_AE, slow);
                a.load(RAX, slot(s));
                a.store(top(0), RAX);
                a.load(RAX, slot(s).at(8));
                a.store(top(0).at(8), RAX);
                a.alu_imm(0, R13, V);
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
                a.mov_imm64(RCX, n as i64);
                a.divide(op);
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
                a.mov_imm64(RCX, n as i64);
                a.divide(op);
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
            Op::Call(index, argc) => {
                a.sync();
                a.mov_rr(RDI, RBX);
                a.mov_imm32(RSI, index);
                a.mov_imm32(RDX, argc);
                a.mov_imm32(RCX, pc);
                a.call(self.helpers.call);
                self.enter_callee();
            }
            Op::CallValue(argc) => {
                a.sync();
                a.mov_rr(RDI, RBX);
                a.mov_imm32(RSI, argc);
                a.mov_imm32(RDX, pc);
                a.call(self.helpers.call_value);
                self.enter_callee();
            }
            Op::Return => {
                a.sync();
                a.mov_rr(RDI, RBX);
                a.call(self.helpers.ret);
                a.buf.extend_from_slice(&[0x31, 0xC0]); // xor eax, eax
                a.jmp(self.exit);
            }
            Op::AndJump(target) | Op::OrJump(target) | Op::ForList(_, target) => {
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
pub fn compile(func: &Func, helpers: &Helpers) -> Vec<u8> {
    let mut a = Asm::default();
    let ops = (0..=func.code.len()).map(|_| a.label()).collect();
    let (exit, fail, status) = (a.label(), a.label(), a.label());
    let mut g = Gen { a, helpers, ops, stubs: Vec::new(), exit, fail, status };

    // Three pushes leave the native stack aligned for calls.
    g.a.push(RBX);
    g.a.push(R12);
    g.a.push(R13);
    g.a.mov_rr(RBX, RDI);
    g.a.reload();
    g.a.lea(R12, M(R13, -(func.nlocals as i32) * V));

    for (pc, op) in func.code.iter().enumerate() {
        g.a.bind(g.ops[pc]);
        g.op(pc as u32, *op);
    }
    g.a.bind(g.ops[func.code.len()]);

    // Leave with the status in eax: 0 for success, 1 for a runtime error.
    g.a.bind(exit);
    g.a.pop(R13);
    g.a.pop(R12);
    g.a.pop(RBX);
    g.a.b(0xC3);
    g.a.bind(fail);
    g.a.mov_imm32(RAX, 1);
    g.a.jmp(exit);
    // A helper answered RETURNED or FAILED.
    g.a.bind(status);
    g.a.buf.extend_from_slice(&[0x83, 0xE8, RETURNED as u8]); // sub eax, RETURNED
    g.a.jmp(exit);

    for stub in std::mem::take(&mut g.stubs) {
        g.a.bind(stub.entry);
        g.run_op(stub.pc, stub.target);
        g.a.jmp(stub.resume);
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
