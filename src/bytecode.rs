//! Bytecode and the compiler that produces it from the AST.
//!
//! Every function, closure and test becomes a `Func`: a flat list of `Op`s for a
//! stack machine. Local variables are resolved to numbered slots here, so the
//! virtual machine in `interp` never looks a name up while running.

use crate::ast::*;
use crate::diag::Span;
use crate::interp::Value;
use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;

/// Jump target that has not been filled in yet, and "no path" / "no target" marker.
pub const NONE: u32 = u32::MAX;

#[derive(Clone, Copy, Debug)]
pub enum Op {
    Int(i64),
    Const(u32),
    Bool(bool),
    Unit,
    /// Push a copy of a local slot.
    Load(u32),
    /// Pop into a local slot.
    Store(u32),
    /// Push the value of a local slot and leave `()` in the slot.
    Take(u32),
    Pop,
    Unary(UnOp),
    Arith(BinOp),
    Cmp(BinOp),
    /// `slot <op> constant`, for the common case of a local and an integer literal.
    ArithSlotInt(BinOp, u32, i32),
    CmpSlotInt(BinOp, u32, i32),
    /// `top <op> constant`.
    ArithInt(BinOp, i32),
    CmpInt(BinOp, i32),
    /// Compare a slot (or the top of the stack) with a constant and jump when
    /// the comparison does not hold.
    JumpUnlessSlotInt(BinOp, u32, i32, u32),
    JumpUnlessInt(BinOp, i32, u32),
    Jump(u32),
    /// Pop; jump unless the value is `true`.
    JumpIfFalse(u32),
    /// `&&`: if the top is `false` keep it and jump, otherwise pop it.
    AndJump(u32),
    /// `||`: if the top is `true` keep it and jump, otherwise pop it.
    OrJump(u32),
    /// Function index, argument count.
    Call(u32, u32),
    /// Callee is on top of the arguments.
    CallValue(u32),
    /// Name constant, argument count.
    CallBuiltin(u32, u32),
    CallMethod(u32, u32),
    Return,
    /// `?`: unwrap `Some`/`Ok`, otherwise return the value. With a target, jump
    /// there instead of returning (the function has postconditions to check).
    Try(u32),
    MakeList(u32),
    /// Push an empty list with room for as many items as the list in a slot.
    NewListFor(u32),
    /// Number of key/value pairs.
    MakeMap(u32),
    /// Index into `Func::structs`.
    MakeStruct(u32),
    /// Index into `Func::enums`, argument count.
    MakeEnum(u32, u32),
    /// Function index, number of captured values on the stack.
    MakeClosure(u32, u32),
    FnRef(u32),
    GetField(u32),
    Index,
    /// Join the top `n` values into a string.
    Concat(u32),
    /// `slot[index] = value`; stack: value, index.
    StoreIndex(u32),
    /// `slot.field = value`.
    StoreField(u32, u32),
    /// Assignment through a longer path; index into `Func::paths`.
    Assign(u32, u32),
    /// `place = place.push(v)` and friends, updating the collection where it is stored.
    InPlace(u32, u32, InPlace),
    /// Counter in `slot`, end in `slot + 1`; jump when the range is empty.
    ForRange(u32, u32),
    /// Increment the counter and jump back while it is below the end.
    ForRangeNext(u32, u32),
    /// List in `slot`, position in `slot + 1`, item goes to `slot + 2`; jump when done.
    ForList(u32, u32),
    /// Remember / restore the stack height, so `break` and `continue` can leave
    /// from the middle of an expression.
    SaveSp(u32),
    RestoreSp(u32),
    /// Slot, variant name constant, argument count. Pushes a Bool.
    IsVariant(u32, u32, u32),
    /// Slot holding an enum value, argument index.
    EnumArg(u32, u32),
    /// Index into `Func::aux` for the source text of the condition.
    Assert(u32),
    AssertCmp(BinOp, u32),
    Require(u32),
    /// Text index, slot holding the result.
    Ensure(u32, u32),
    /// A state the type checker rules out.
    Internal,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum InPlace {
    Push,
    Set,
    Remove,
}

impl InPlace {
    pub fn method(self) -> &'static str {
        match self {
            InPlace::Push => "push",
            InPlace::Set => "set",
            InPlace::Remove => "remove",
        }
    }
}

pub enum PathElem {
    Field(Rc<str>, Span),
    Index(Span),
}

pub struct StructTmpl {
    pub name: Rc<str>,
    /// Fields in declaration order, each with its position among the pushed values.
    pub fields: Vec<(Rc<str>, usize)>,
    pub argc: usize,
}

#[derive(Default)]
pub struct Func {
    pub name: String,
    pub nlocals: usize,
    /// The function has no return type, so it always returns `()`.
    pub unit_ret: bool,
    pub code: Vec<Op>,
    /// Source position of each op, for runtime errors.
    pub spans: Vec<Span>,
    pub consts: Vec<Value>,
    pub aux: Vec<Span>,
    pub structs: Vec<StructTmpl>,
    pub enums: Vec<(Rc<str>, Rc<str>)>,
    pub paths: Vec<Vec<PathElem>>,
    /// Address of the machine code generated for this function, or 0.
    pub native: Cell<usize>,
    /// Free value-stack cells the function needs before it may be entered.
    /// Set together with `native`.
    pub need: Cell<usize>,
    /// The function is declared to return `Int`.
    pub int_ret: bool,
    /// Number of declared parameters (0 for closures and tests).
    pub nparams: usize,
    /// The function declares no effects, so its result depends only on its arguments.
    pub pure_fn: bool,
    /// Address of the function's result cache, or 0 (see `jit`).
    pub memo: Cell<usize>,
    /// For each local slot: it only ever holds an `Int`.
    pub int_slots: Vec<bool>,
    /// For each op: it is an `Arith` or `Cmp` whose operands are `Int`.
    pub int_ops: Vec<bool>,
}

/// Everything shared between the functions of one program.
#[derive(Default)]
pub struct Globals {
    pub fn_index: HashMap<String, u32>,
    pub struct_fields: HashMap<String, Vec<String>>,
    pub funcs: Vec<Rc<Func>>,
    pub types: TypeInfo,
}

pub fn compile_fn(g: &mut Globals, f: &FnDecl) -> Func {
    let mut c = Compiler::new(g, &f.name);
    c.func.unit_ret = f.ret.is_none();
    c.func.int_ret = c.g.types.int_fns.contains(&f.name);
    c.func.nparams = f.params.len();
    c.func.pure_fn = f.effects.is_empty();
    c.push_scope();
    for p in &f.params {
        let slot = c.alloc_var(p.span);
        c.bind(&p.name, slot);
    }
    for r in &f.requires {
        c.expr(r);
        let aux = c.aux(r.span);
        c.emit(Op::Require(aux), r.span);
    }
    if !f.ensures.is_empty() {
        c.landing = Some(Vec::new());
    }
    c.block(&f.body);
    if let Some(jumps) = c.landing.take() {
        let here = c.here();
        for j in jumps {
            c.patch(j, here);
        }
        let span = f.body.span;
        if c.func.unit_ret {
            c.emit(Op::Pop, span);
            c.emit(Op::Unit, span);
        }
        // Only the parameters and `result` are visible to postconditions.
        c.push_scope();
        let result = c.alloc();
        c.bind("result", result);
        c.emit(Op::Store(result), span);
        for e in &f.ensures {
            c.expr(e);
            let aux = c.aux(e.span);
            c.emit(Op::Ensure(aux, result), e.span);
        }
        c.emit(Op::Load(result), span);
    }
    c.emit(Op::Return, f.body.span);
    c.func
}

pub fn compile_test(g: &mut Globals, t: &TestDecl) -> Func {
    let mut c = Compiler::new(g, &t.name);
    c.push_scope();
    c.block(&t.body);
    c.emit(Op::Return, t.body.span);
    c.func
}

struct Loop {
    sp_slot: u32,
    breaks: Vec<usize>,
    continues: Vec<usize>,
}

struct Scope {
    names: Vec<(String, u32)>,
    /// Value of `next_slot` when the scope was opened.
    first_slot: u32,
}

struct Compiler<'g> {
    g: &'g mut Globals,
    func: Func,
    scopes: Vec<Scope>,
    next_slot: u32,
    loops: Vec<Loop>,
    /// Set while compiling a function with postconditions: returns jump to the
    /// code that checks them, and are recorded here until its address is known.
    landing: Option<Vec<usize>>,
    strings: HashMap<String, u32>,
}

impl<'g> Compiler<'g> {
    fn new(g: &'g mut Globals, name: &str) -> Compiler<'g> {
        Compiler {
            g,
            func: Func { name: name.to_string(), ..Func::default() },
            scopes: Vec::new(),
            next_slot: 0,
            loops: Vec::new(),
            landing: None,
            strings: HashMap::new(),
        }
    }

    // ---- emitting ----

    fn emit(&mut self, op: Op, span: Span) -> usize {
        self.func.code.push(op);
        self.func.spans.push(span);
        self.func.int_ops.push(false);
        self.func.code.len() - 1
    }

    fn here(&self) -> u32 {
        self.func.code.len() as u32
    }

    fn patch(&mut self, at: usize, target: u32) {
        match &mut self.func.code[at] {
            Op::Jump(t)
            | Op::JumpIfFalse(t)
            | Op::AndJump(t)
            | Op::OrJump(t)
            | Op::Try(t)
            | Op::ForRange(_, t)
            | Op::ForRangeNext(_, t)
            | Op::ForList(_, t)
            | Op::JumpUnlessSlotInt(_, _, _, t)
            | Op::JumpUnlessInt(_, _, t) => *t = target,
            other => unreachable!("cannot patch {other:?}"),
        }
    }

    fn patch_here(&mut self, at: usize) {
        let here = self.here();
        self.patch(at, here);
    }

    fn konst(&mut self, v: Value) -> u32 {
        self.func.consts.push(v);
        (self.func.consts.len() - 1) as u32
    }

    fn string(&mut self, s: &str) -> u32 {
        if let Some(&i) = self.strings.get(s) {
            return i;
        }
        let i = self.konst(Value::Str(Rc::from(s)));
        self.strings.insert(s.to_string(), i);
        i
    }

    fn aux(&mut self, span: Span) -> u32 {
        self.func.aux.push(span);
        (self.func.aux.len() - 1) as u32
    }

    // ---- scopes and slots ----

    fn push_scope(&mut self) {
        self.scopes.push(Scope { names: Vec::new(), first_slot: self.next_slot });
    }

    fn pop_scope(&mut self) {
        let scope = self.scopes.pop().expect("scope");
        self.next_slot = scope.first_slot;
    }

    /// A slot for a value of any type.
    fn alloc(&mut self) -> u32 {
        self.alloc_kind(false)
    }

    /// A slot for the variable declared at `span`.
    fn alloc_var(&mut self, span: Span) -> u32 {
        let int = self.g.types.int_vars.contains(&span.start);
        self.alloc_kind(int)
    }

    /// Slots are reused once their scope ends, so a slot counts as an `Int`
    /// slot only if every variable that ever lives in it is an `Int`.
    fn alloc_kind(&mut self, int: bool) -> u32 {
        let slot = self.next_slot;
        self.next_slot += 1;
        self.func.nlocals = self.func.nlocals.max(self.next_slot as usize);
        match self.func.int_slots.get_mut(slot as usize) {
            Some(known) => *known = *known && int,
            None => self.func.int_slots.push(int),
        }
        slot
    }

    fn bind(&mut self, name: &str, slot: u32) {
        if name != "_" {
            self.scopes.last_mut().expect("scope").names.push((name.to_string(), slot));
        }
    }

    fn resolve(&self, name: &str) -> Option<u32> {
        self.scopes
            .iter()
            .rev()
            .find_map(|s| s.names.iter().rev().find(|(n, _)| n == name).map(|(_, slot)| *slot))
    }

    // ---- statements ----

    /// Compiles a block whose value is not used. Leaves nothing on the stack.
    fn block_discard(&mut self, block: &Block) {
        self.push_scope();
        for stmt in &block.stmts {
            self.stmt(stmt);
        }
        self.pop_scope();
    }

    /// Compiles an expression whose value is not used. Leaves nothing on the stack.
    fn expr_discard(&mut self, e: &Expr) {
        match &e.kind {
            ExprKind::If { cond, then, els } => {
                let to_else = self.cond_jump(cond);
                self.block_discard(then);
                match els {
                    Some(els) => {
                        let to_end = self.emit(Op::Jump(NONE), e.span);
                        self.patch_here(to_else);
                        self.expr_discard(els);
                        self.patch_here(to_end);
                    }
                    None => self.patch_here(to_else),
                }
            }
            ExprKind::Block(b) => self.block_discard(b),
            _ => {
                self.expr(e);
                self.emit(Op::Pop, e.span);
            }
        }
    }

    /// Compiles a condition and the jump taken when it is not `true`.
    /// Returns the jump, to be patched with its target.
    fn cond_jump(&mut self, cond: &Expr) -> usize {
        if let ExprKind::Binary(op, lhs, rhs) = &cond.kind {
            if let (true, Some(n)) = (op.is_comparison(), small_int(rhs)) {
                if let Some(slot) = self.local(lhs) {
                    return self.emit(Op::JumpUnlessSlotInt(*op, slot, n, NONE), cond.span);
                }
                self.expr(lhs);
                return self.emit(Op::JumpUnlessInt(*op, n, NONE), cond.span);
            }
        }
        self.expr(cond);
        self.emit(Op::JumpIfFalse(NONE), cond.span)
    }

    /// Compiles a block. Leaves its value on the stack.
    fn block(&mut self, block: &Block) {
        self.push_scope();
        let last = block.stmts.len().wrapping_sub(1);
        for (i, stmt) in block.stmts.iter().enumerate() {
            match stmt {
                Stmt::Expr(e) => {
                    if i != last {
                        self.expr_discard(e);
                    } else {
                        self.expr(e);
                    }
                }
                other => {
                    self.stmt(other);
                    if i == last {
                        self.emit(Op::Unit, other.span());
                    }
                }
            }
        }
        if block.stmts.is_empty() {
            self.emit(Op::Unit, block.span);
        }
        self.pop_scope();
    }

    fn stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Let { name, name_span, value, span, .. } => {
                self.expr(value);
                if name == "_" {
                    self.emit(Op::Pop, *span);
                } else {
                    let slot = self.alloc_var(*name_span);
                    self.bind(name, slot);
                    self.emit(Op::Store(slot), *span);
                }
            }
            Stmt::Assign { target, value } => self.assign(target, value),
            Stmt::While { cond, body, span } => {
                self.push_scope();
                let sp_slot = self.alloc();
                self.emit(Op::SaveSp(sp_slot), *span);
                let top = self.here();
                let exit = self.cond_jump(cond);
                self.loops.push(Loop { sp_slot, breaks: Vec::new(), continues: Vec::new() });
                self.block_discard(body);
                self.emit(Op::Jump(top), *span);
                let l = self.loops.pop().expect("loop");
                for c in l.continues {
                    self.patch(c, top);
                }
                self.patch_here(exit);
                for b in l.breaks {
                    self.patch_here(b);
                }
                self.pop_scope();
            }
            Stmt::For { var, iter, end: Some(end), body, span, .. } => {
                self.push_scope();
                // A range always counts in `Int`.
                let counter = self.alloc_kind(true);
                let limit = self.alloc_kind(true);
                let sp_slot = self.alloc();
                self.expr(iter);
                self.expr(end);
                self.emit(Op::Store(limit), end.span);
                self.emit(Op::Store(counter), iter.span);
                self.bind(var, counter);
                self.emit(Op::SaveSp(sp_slot), *span);
                let exit = self.emit(Op::ForRange(counter, NONE), iter.span);
                let top = self.here();
                self.loops.push(Loop { sp_slot, breaks: Vec::new(), continues: Vec::new() });
                self.block_discard(body);
                let next = self.here();
                self.emit(Op::ForRangeNext(counter, top), *span);
                let l = self.loops.pop().expect("loop");
                for c in l.continues {
                    self.patch(c, next);
                }
                self.patch_here(exit);
                for b in l.breaks {
                    self.patch_here(b);
                }
                self.pop_scope();
            }
            Stmt::For { var, iter, end: None, body, span, .. } => {
                self.push_scope();
                let list = self.alloc();
                let pos = self.alloc();
                let item = self.alloc();
                let sp_slot = self.alloc();
                self.expr(iter);
                self.emit(Op::Store(list), iter.span);
                self.emit(Op::Int(0), iter.span);
                self.emit(Op::Store(pos), iter.span);
                self.bind(var, item);
                self.emit(Op::SaveSp(sp_slot), *span);
                let top = self.here();
                let exit = self.emit(Op::ForList(list, NONE), iter.span);
                self.loops.push(Loop { sp_slot, breaks: Vec::new(), continues: Vec::new() });
                self.block_discard(body);
                self.emit(Op::Jump(top), *span);
                let l = self.loops.pop().expect("loop");
                for c in l.continues {
                    self.patch(c, top);
                }
                self.patch_here(exit);
                for b in l.breaks {
                    self.patch_here(b);
                }
                // Release the list so that later updates to it need no copy.
                self.emit(Op::Unit, *span);
                self.emit(Op::Store(list), *span);
                self.pop_scope();
            }
            Stmt::Assert { cond, span } => {
                let aux = self.aux(cond.span);
                if let ExprKind::Binary(op, lhs, rhs) = &cond.kind {
                    if op.is_comparison() {
                        self.expr(lhs);
                        self.expr(rhs);
                        self.emit(Op::AssertCmp(*op, aux), *span);
                        return;
                    }
                }
                self.expr(cond);
                self.emit(Op::Assert(aux), *span);
            }
            Stmt::Expr(e) => self.expr_discard(e),
        }
    }

    fn assign(&mut self, target: &Expr, value: &Expr) {
        if self.assign_in_place(target, value) {
            return;
        }
        self.expr(value);
        let span = target.span;
        match &target.kind {
            ExprKind::Ident(name) => match self.resolve(name) {
                Some(slot) => {
                    self.emit(Op::Store(slot), span);
                }
                None => {
                    self.emit(Op::Internal, span);
                }
            },
            _ => {
                let mut path = Vec::new();
                let mut indexes = Vec::new();
                let Some(slot) = self.place(target, &mut path, &mut indexes) else {
                    self.emit(Op::Internal, span);
                    return;
                };
                match (path.as_slice(), indexes.as_slice()) {
                    ([PathElem::Index(_)], [index]) => {
                        self.expr(index);
                        self.emit(Op::StoreIndex(slot), span);
                    }
                    ([PathElem::Field(name, _)], []) => {
                        let name = self.string(name);
                        self.emit(Op::StoreField(slot, name), span);
                    }
                    _ => {
                        // Outermost index first, so the machine pops them from the root down.
                        for index in indexes.iter().rev() {
                            self.expr(index);
                        }
                        self.func.paths.push(path);
                        let path = (self.func.paths.len() - 1) as u32;
                        self.emit(Op::Assign(slot, path), span);
                    }
                }
            }
        }
    }

    /// Breaks an assignment target into its root variable and the path below it.
    fn place<'e>(&self, target: &'e Expr, path: &mut Vec<PathElem>, indexes: &mut Vec<&'e Expr>) -> Option<u32> {
        match &target.kind {
            ExprKind::Ident(name) => self.resolve(name),
            ExprKind::Field(base, name, _) => {
                let slot = self.place(base, path, indexes)?;
                path.push(PathElem::Field(Rc::from(name.as_str()), target.span));
                Some(slot)
            }
            ExprKind::Index(base, index) => {
                let slot = self.place(base, path, indexes)?;
                path.push(PathElem::Index(target.span));
                indexes.push(index);
                Some(slot)
            }
            _ => None,
        }
    }

    /// `x = x.push(v)`, `m = m.set(k, v)` and `m = m.remove(k)` update the
    /// collection where it is stored instead of rebuilding it. The machine still
    /// copies when the collection is shared, so other holders never see the change.
    fn assign_in_place(&mut self, target: &Expr, value: &Expr) -> bool {
        let ExprKind::MethodCall { recv, name, args, .. } = &value.kind else {
            return false;
        };
        let (kind, arity) = match name.as_str() {
            "push" => (InPlace::Push, 1),
            "remove" => (InPlace::Remove, 1),
            "set" => (InPlace::Set, 2),
            _ => return false,
        };
        if args.len() != arity || !same_place(target, recv) {
            return false;
        }
        let mut path = Vec::new();
        let mut indexes = Vec::new();
        let Some(slot) = self.place(target, &mut path, &mut indexes) else {
            return false;
        };
        // Arguments cannot modify the target, so evaluating them first is not observable.
        for a in args {
            self.expr(a);
        }
        let path = if path.is_empty() {
            NONE
        } else {
            self.func.paths.push(path);
            (self.func.paths.len() - 1) as u32
        };
        self.emit(Op::InPlace(slot, path, kind), value.span);
        true
    }

    /// `map`, `filter`, `fold`, `any` and `all` exist only on lists, so they
    /// are compiled as loops that call the function once per item. The
    /// machine then runs them like any other loop, without re-entering itself.
    fn list_loop(&mut self, recv: &Expr, name: &str, args: &[Expr], span: Span) -> bool {
        let arity = match name {
            "map" | "filter" | "any" | "all" => 1,
            "fold" => 2,
            _ => return false,
        };
        if args.len() != arity {
            return false;
        }
        self.push_scope();
        // `ForList` expects the list, the position and the item in adjacent slots.
        let list = self.alloc();
        let pos = self.alloc();
        let item = self.alloc();
        let func = self.alloc();
        // The result so far: the output list, the accumulator or the answer.
        let out = self.alloc();

        self.expr(recv);
        self.emit(Op::Store(list), span);
        if name == "fold" {
            self.expr(&args[0]);
            self.emit(Op::Store(out), span);
        }
        self.expr(&args[arity - 1]);
        self.emit(Op::Store(func), span);
        let initial = match name {
            "map" => Some(Op::NewListFor(list)),
            "filter" => Some(Op::MakeList(0)),
            "any" => Some(Op::Bool(false)),
            "all" => Some(Op::Bool(true)),
            _ => None,
        };
        if let Some(op) = initial {
            self.emit(op, span);
            self.emit(Op::Store(out), span);
        }
        self.emit(Op::Int(0), span);
        self.emit(Op::Store(pos), span);

        let top = self.here();
        let exit = self.emit(Op::ForList(list, NONE), span);
        if name == "fold" {
            // Taking the accumulator keeps it unshared, so the function may update it in place.
            self.emit(Op::Take(out), span);
        }
        self.emit(Op::Load(item), span);
        self.emit(Op::Load(func), span);
        self.emit(Op::CallValue(arity as u32), span);
        match name {
            "map" => {
                self.emit(Op::InPlace(out, NONE, InPlace::Push), span);
                self.emit(Op::Jump(top), span);
            }
            "filter" => {
                self.emit(Op::JumpIfFalse(top), span);
                self.emit(Op::Load(item), span);
                self.emit(Op::InPlace(out, NONE, InPlace::Push), span);
                self.emit(Op::Jump(top), span);
            }
            "fold" => {
                self.emit(Op::Store(out), span);
                self.emit(Op::Jump(top), span);
            }
            "any" => {
                // The first `true` decides the answer.
                self.emit(Op::JumpIfFalse(top), span);
                self.emit(Op::Bool(true), span);
                self.emit(Op::Store(out), span);
            }
            _ => {
                // The first `false` decides the answer.
                let stop = self.emit(Op::JumpIfFalse(NONE), span);
                self.emit(Op::Jump(top), span);
                self.patch_here(stop);
                self.emit(Op::Bool(false), span);
                self.emit(Op::Store(out), span);
            }
        }
        self.patch_here(exit);
        // Release what the loop held, so that later updates need no copy.
        for slot in [list, item, func] {
            self.emit(Op::Unit, span);
            self.emit(Op::Store(slot), span);
        }
        self.emit(Op::Take(out), span);
        self.pop_scope();
        true
    }

    // ---- expressions ----

    fn args(&mut self, args: &[Expr]) {
        for a in args {
            self.expr(a);
        }
    }

    /// Compiles an expression. Leaves its value on the stack.
    fn expr(&mut self, e: &Expr) {
        let span = e.span;
        match &e.kind {
            ExprKind::Int(n) => {
                self.emit(Op::Int(*n), span);
            }
            ExprKind::Float(f) => {
                let k = self.konst(Value::Float(*f));
                self.emit(Op::Const(k), span);
            }
            ExprKind::Bool(b) => {
                self.emit(Op::Bool(*b), span);
            }
            ExprKind::Unit => {
                self.emit(Op::Unit, span);
            }
            ExprKind::Str(parts) => match parts.as_slice() {
                [] => {
                    let k = self.string("");
                    self.emit(Op::Const(k), span);
                }
                [StrPart::Lit(s)] => {
                    let k = self.string(s);
                    self.emit(Op::Const(k), span);
                }
                parts => {
                    for part in parts {
                        match part {
                            StrPart::Lit(s) => {
                                let k = self.string(s);
                                self.emit(Op::Const(k), span);
                            }
                            StrPart::Expr(inner) => self.expr(inner),
                        }
                    }
                    self.emit(Op::Concat(parts.len() as u32), span);
                }
            },
            ExprKind::Ident(name) => {
                let op = self.resolve(name).map(Op::Load).unwrap_or(Op::Internal);
                self.emit(op, span);
            }
            ExprKind::FnRef(name) => {
                let op = self.g.fn_index.get(name).map(|i| Op::FnRef(*i)).unwrap_or(Op::Internal);
                self.emit(op, span);
            }
            ExprKind::List(items) => {
                self.args(items);
                self.emit(Op::MakeList(items.len() as u32), span);
            }
            ExprKind::MapLit(entries) => {
                for (k, v) in entries {
                    self.expr(k);
                    self.expr(v);
                }
                self.emit(Op::MakeMap(entries.len() as u32), span);
            }
            ExprKind::Unary(op, operand) => {
                self.expr(operand);
                self.emit(Op::Unary(*op), span);
            }
            ExprKind::Binary(op @ (BinOp::And | BinOp::Or), lhs, rhs) => {
                self.expr(lhs);
                let jump = match op {
                    BinOp::And => self.emit(Op::AndJump(NONE), span),
                    _ => self.emit(Op::OrJump(NONE), span),
                };
                self.expr(rhs);
                self.patch_here(jump);
            }
            ExprKind::Binary(op, lhs, rhs) => {
                if let (Some(slot), Some(n)) = (self.local(lhs), small_int(rhs)) {
                    let op = if op.is_comparison() {
                        Op::CmpSlotInt(*op, slot, n)
                    } else {
                        Op::ArithSlotInt(*op, slot, n)
                    };
                    self.emit(op, span);
                    return;
                }
                self.expr(lhs);
                if let Some(n) = small_int(rhs) {
                    let op = if op.is_comparison() { Op::CmpInt(*op, n) } else { Op::ArithInt(*op, n) };
                    self.emit(op, span);
                    return;
                }
                self.expr(rhs);
                let int = self.g.types.int_ops.contains(&(span.start, span.end));
                let op = if op.is_comparison() { Op::Cmp(*op) } else { Op::Arith(*op) };
                let at = self.emit(op, span);
                self.func.int_ops[at] = int;
            }
            ExprKind::Call { name, local, args, .. } => {
                self.args(args);
                let argc = args.len() as u32;
                if *local {
                    match self.resolve(name) {
                        Some(slot) => {
                            self.emit(Op::Load(slot), span);
                            self.emit(Op::CallValue(argc), span);
                        }
                        None => {
                            self.emit(Op::Internal, span);
                        }
                    }
                } else if let Some(&index) = self.g.fn_index.get(name) {
                    self.emit(Op::Call(index, argc), span);
                } else {
                    let name = self.string(name);
                    self.emit(Op::CallBuiltin(name, argc), span);
                }
            }
            ExprKind::MethodCall { recv, name, args, .. } => {
                if self.list_loop(recv, name, args, span) {
                    return;
                }
                self.expr(recv);
                self.args(args);
                let name = self.string(name);
                self.emit(Op::CallMethod(name, args.len() as u32), span);
            }
            ExprKind::Field(base, name, _) => {
                self.expr(base);
                let name = self.string(name);
                self.emit(Op::GetField(name), span);
            }
            ExprKind::Index(base, index) => {
                self.expr(base);
                self.expr(index);
                self.emit(Op::Index, span);
            }
            ExprKind::StructLit { name, fields } => {
                for (_, _, value) in fields {
                    self.expr(value);
                }
                // Store fields in declaration order so equality and display are stable.
                let order = self.g.struct_fields.get(name).cloned().unwrap_or_default();
                let mut ordered = Vec::with_capacity(order.len());
                for fname in &order {
                    match fields.iter().position(|(n, _, _)| n == fname) {
                        Some(i) => ordered.push((Rc::from(fname.as_str()), i)),
                        None => {
                            self.emit(Op::Internal, span);
                            return;
                        }
                    }
                }
                self.func.structs.push(StructTmpl { name: Rc::from(name.as_str()), fields: ordered, argc: fields.len() });
                self.emit(Op::MakeStruct((self.func.structs.len() - 1) as u32), span);
            }
            ExprKind::Ctor { enum_name, variant, args } => {
                let (enum_name, variant): (Rc<str>, Rc<str>) = (Rc::from(enum_name.as_str()), Rc::from(variant.as_str()));
                if args.is_empty() {
                    let k = self.konst(Value::variant(enum_name, variant, Vec::new()));
                    self.emit(Op::Const(k), span);
                } else {
                    self.args(args);
                    self.func.enums.push((enum_name, variant));
                    self.emit(Op::MakeEnum((self.func.enums.len() - 1) as u32, args.len() as u32), span);
                }
            }
            ExprKind::If { cond, then, els } => {
                let to_else = self.cond_jump(cond);
                self.block(then);
                let to_end = self.emit(Op::Jump(NONE), span);
                self.patch_here(to_else);
                match els {
                    Some(els) => self.expr(els),
                    None => {
                        self.emit(Op::Unit, span);
                    }
                }
                self.patch_here(to_end);
            }
            ExprKind::Match { scrutinee, arms } => {
                self.push_scope();
                self.expr(scrutinee);
                let subject = self.alloc();
                self.emit(Op::Store(subject), scrutinee.span);
                let mut to_end = Vec::new();
                for arm in arms {
                    self.push_scope();
                    let mut fails = Vec::new();
                    self.pattern(&arm.pat, subject, &mut fails);
                    self.expr(&arm.body);
                    to_end.push(self.emit(Op::Jump(NONE), span));
                    for f in fails {
                        self.patch_here(f);
                    }
                    self.pop_scope();
                }
                self.emit(Op::Internal, span);
                for j in to_end {
                    self.patch_here(j);
                }
                self.pop_scope();
            }
            ExprKind::Block(b) => self.block(b),
            ExprKind::Closure { params, captures, body } => {
                let captured: Vec<(&String, u32, bool)> =
                    captures
                    .iter()
                    .filter_map(|name| self.resolve(name).map(|slot| (name, slot, self.func.int_slots[slot as usize])))
                    .collect();
                let func = {
                    let mut c = Compiler::new(&mut *self.g, "<closure>");
                    c.push_scope();
                    let slots: Vec<u32> = params.iter().map(|p| c.alloc_var(p.span)).collect();
                    for (name, _, int) in &captured {
                        let slot = c.alloc_kind(*int);
                        c.bind(name, slot);
                    }
                    // Bound last, so a parameter shadows a captured variable of the same name.
                    for (p, slot) in params.iter().zip(slots) {
                        c.bind(&p.name, slot);
                    }
                    c.expr(body);
                    c.emit(Op::Return, body.span);
                    c.func
                };
                self.g.funcs.push(Rc::new(func));
                let index = (self.g.funcs.len() - 1) as u32;
                for (_, slot, _) in &captured {
                    self.emit(Op::Load(*slot), span);
                }
                self.emit(Op::MakeClosure(index, captured.len() as u32), span);
            }
            ExprKind::Return(value) => {
                match value {
                    Some(v) => self.expr(v),
                    None => {
                        self.emit(Op::Unit, span);
                    }
                }
                if self.landing.is_some() {
                    let j = self.emit(Op::Jump(NONE), span);
                    self.landing.as_mut().expect("landing").push(j);
                } else {
                    self.emit(Op::Return, span);
                }
            }
            ExprKind::Break | ExprKind::Continue => {
                let Some(sp_slot) = self.loops.last().map(|l| l.sp_slot) else {
                    self.emit(Op::Internal, span);
                    return;
                };
                self.emit(Op::RestoreSp(sp_slot), span);
                let j = self.emit(Op::Jump(NONE), span);
                let l = self.loops.last_mut().expect("loop");
                match e.kind {
                    ExprKind::Break => l.breaks.push(j),
                    _ => l.continues.push(j),
                }
            }
            ExprKind::Try(inner) => {
                self.expr(inner);
                let j = self.emit(Op::Try(NONE), span);
                if let Some(landing) = &mut self.landing {
                    landing.push(j);
                }
            }
        }
    }

    fn local(&self, e: &Expr) -> Option<u32> {
        match &e.kind {
            ExprKind::Ident(name) => self.resolve(name),
            _ => None,
        }
    }

    /// Tests the value in `src` against a pattern, binding its variables.
    /// Jumps that leave when the pattern does not match are added to `fails`.
    fn pattern(&mut self, pat: &Pattern, src: u32, fails: &mut Vec<usize>) {
        let span = pat.span;
        let literal = |c: &mut Compiler, op: Op, fails: &mut Vec<usize>| {
            c.emit(Op::Load(src), span);
            c.emit(op, span);
            c.emit(Op::Cmp(BinOp::Eq), span);
            fails.push(c.emit(Op::JumpIfFalse(NONE), span));
        };
        match &pat.kind {
            PatKind::Wildcard => {}
            PatKind::Bind(name) => self.bind(name, src),
            PatKind::Int(n) => literal(self, Op::Int(*n), fails),
            PatKind::Bool(b) => literal(self, Op::Bool(*b), fails),
            PatKind::Str(s) => {
                let k = self.string(s);
                literal(self, Op::Const(k), fails);
            }
            PatKind::Ctor { variant, args, .. } => {
                let name = self.string(variant);
                self.emit(Op::IsVariant(src, name, args.len() as u32), span);
                fails.push(self.emit(Op::JumpIfFalse(NONE), span));
                for (i, arg) in args.iter().enumerate() {
                    if matches!(arg.kind, PatKind::Wildcard) {
                        continue;
                    }
                    let slot = self.alloc();
                    self.emit(Op::EnumArg(src, i as u32), arg.span);
                    self.emit(Op::Store(slot), arg.span);
                    self.pattern(arg, slot, fails);
                }
            }
        }
    }
}

fn small_int(e: &Expr) -> Option<i32> {
    match e.kind {
        ExprKind::Int(n) => i32::try_from(n).ok(),
        _ => None,
    }
}

/// True when both expressions name the same variable or the same field path of one.
fn same_place(a: &Expr, b: &Expr) -> bool {
    match (&a.kind, &b.kind) {
        (ExprKind::Ident(x), ExprKind::Ident(y)) => x == y,
        (ExprKind::Field(ab, an, _), ExprKind::Field(bb, bn, _)) => an == bn && same_place(ab, bb),
        _ => false,
    }
}
