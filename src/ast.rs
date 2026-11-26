//! Abstract syntax tree.
//!
//! When a program is loaded for checking and running, the parser rewrites every
//! reference to a top-level declaration into its canonical, module-qualified
//! name (`geometry.shapes.area`), so later stages never deal with imports.

use crate::diag::Span;
use std::rc::Rc;

pub const BUILTIN_FNS: &[&str] = &[
    "print",
    "read_line",
    "panic",
    "read_file",
    "write_file",
    "args",
    // A window to draw in, input, the clock and random numbers.
    "window_open",
    "window_update",
    "window_close",
    "draw_clear",
    "draw_pixel",
    "draw_rect",
    "rgb",
    "key_down",
    "mouse_x",
    "mouse_y",
    "mouse_down",
    "time_ms",
    "sleep_ms",
    "random",
    "random_seed",
];
pub const BUILTIN_TYPES: &[&str] =
    &["Int", "Float", "Bool", "String", "Unit", "List", "Option", "Result", "Map"];
pub const EFFECTS: &[&str] = &["io", "fs"];

pub struct Program {
    pub imports: Vec<Import>,
    pub items: Vec<Item>,
    /// What the type checker learned, for the code generator.
    pub types: TypeInfo,
}

/// Static types the code generator relies on. Everything not listed here is
/// handled without assuming a type.
#[derive(Clone, Default)]
pub struct TypeInfo {
    /// Variables of type `Int`, by the start of the span they are declared at.
    pub int_vars: std::collections::HashSet<usize>,
    /// Binary operations whose operands are `Int`, by span.
    pub int_ops: std::collections::HashSet<(usize, usize)>,
    /// Functions that return `Int`, by name.
    pub int_fns: std::collections::HashSet<String>,
    /// The type of every declared variable, written as in source (`List<Int>`),
    /// by the start of the span it is declared at.
    pub var_types: std::collections::HashMap<usize, String>,
    /// Only when asked for (`checker::RECORD_EXPR_TYPES`): the type of every
    /// expression that is not a plain literal or variable, written as in
    /// source, by span.
    pub expr_types: std::collections::HashMap<(usize, usize), String>,
    /// Likewise only when asked for: for each use of a generic function, by
    /// span, the types its type parameters stand for there.
    pub call_types: std::collections::HashMap<(usize, usize), Vec<String>>,
    /// For each field access `a.b`, by span: the position of the field in its struct.
    pub field_index: std::collections::HashMap<(usize, usize), u32>,
}

pub struct Import {
    /// Dotted module path, e.g. `geometry.shapes`.
    pub path: String,
    /// Last segment of the path; the name used to refer to the module.
    pub alias: String,
    pub span: Span,
}

pub enum Item {
    Fn(Rc<FnDecl>),
    Struct(StructDecl),
    Enum(EnumDecl),
    Test(Rc<TestDecl>),
}

impl Item {
    pub fn span(&self) -> Span {
        match self {
            Item::Fn(f) => f.span,
            Item::Struct(s) => s.span,
            Item::Enum(e) => e.span,
            Item::Test(t) => t.span,
        }
    }
}

pub struct Param {
    pub name: String,
    pub ty: TypeExpr,
    pub span: Span,
}

pub struct FnDecl {
    pub name: String,
    pub name_span: Span,
    pub span: Span,
    pub type_params: Vec<String>,
    pub params: Vec<Param>,
    pub ret: Option<TypeExpr>,
    pub effects: Vec<(String, Span)>,
    pub requires: Vec<Expr>,
    pub ensures: Vec<Expr>,
    pub body: Block,
}

pub struct StructDecl {
    pub name: String,
    pub name_span: Span,
    pub span: Span,
    pub type_params: Vec<String>,
    pub fields: Vec<Param>,
}

pub struct EnumDecl {
    pub name: String,
    pub name_span: Span,
    pub span: Span,
    pub type_params: Vec<String>,
    pub variants: Vec<Variant>,
}

pub struct Variant {
    pub name: String,
    pub span: Span,
    pub fields: Vec<TypeExpr>,
}

pub struct TestDecl {
    pub name: String,
    pub name_span: Span,
    pub span: Span,
    pub effects: Vec<(String, Span)>,
    pub body: Block,
    /// True for tests in the entry file; only these are run.
    pub root: bool,
}

pub struct TypeExpr {
    pub kind: TypeKind,
    pub span: Span,
}

pub enum TypeKind {
    Named(String, Vec<TypeExpr>),
    /// `fn(params) -> ret uses effects`
    Fn(Vec<TypeExpr>, Option<Box<TypeExpr>>, Vec<String>),
}

impl std::fmt::Display for TypeExpr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let list = |items: &[TypeExpr]| items.iter().map(|t| t.to_string()).collect::<Vec<_>>().join(", ");
        match &self.kind {
            TypeKind::Named(name, args) => {
                write!(f, "{name}")?;
                if !args.is_empty() {
                    write!(f, "<{}>", list(args))?;
                }
                Ok(())
            }
            TypeKind::Fn(params, ret, effects) => {
                write!(f, "fn({})", list(params))?;
                if let Some(ret) = ret {
                    // A function type in return position needs parentheses so that
                    // a following `uses` clause is not attached to it.
                    match ret.kind {
                        TypeKind::Fn(..) => write!(f, " -> ({ret})")?,
                        _ => write!(f, " -> {ret}")?,
                    }
                }
                if !effects.is_empty() {
                    write!(f, " uses {}", effects.join(", "))?;
                }
                Ok(())
            }
        }
    }
}

pub struct Block {
    pub stmts: Vec<Stmt>,
    pub span: Span,
}

pub enum Stmt {
    Let { name: String, name_span: Span, mutable: bool, ty: Option<TypeExpr>, value: Expr, span: Span },
    Assign { target: Expr, value: Expr },
    While { cond: Expr, body: Block, span: Span },
    /// `for var in iter { .. }` or, when `end` is set, `for var in iter..end { .. }`.
    For { var: String, var_span: Span, iter: Expr, end: Option<Expr>, body: Block, span: Span },
    Assert { cond: Expr, span: Span },
    Expr(Expr),
}

impl Stmt {
    pub fn span(&self) -> Span {
        match self {
            Stmt::Let { span, .. } | Stmt::While { span, .. } | Stmt::For { span, .. } | Stmt::Assert { span, .. } => {
                *span
            }
            Stmt::Assign { target, value } => target.span.to(value.span),
            Stmt::Expr(e) => e.span,
        }
    }
}

pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

pub enum StrPart {
    Lit(String),
    Expr(Expr),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UnOp {
    Neg,
    Not,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

impl BinOp {
    pub fn symbol(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Rem => "%",
            BinOp::Eq => "==",
            BinOp::Ne => "!=",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
            BinOp::And => "&&",
            BinOp::Or => "||",
        }
    }

    pub fn is_comparison(self) -> bool {
        matches!(self, BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge)
    }
}

pub struct ClosureParam {
    pub name: String,
    pub span: Span,
    pub ty: Option<TypeExpr>,
}

pub enum ExprKind {
    Int(i64),
    Float(f64),
    Bool(bool),
    Unit,
    Str(Vec<StrPart>),
    /// A local variable.
    Ident(String),
    /// A top-level function used as a value (also any name that is not a local).
    FnRef(String),
    List(Vec<Expr>),
    MapLit(Vec<(Expr, Expr)>),
    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    /// `local` is true when `name` is a local variable holding a function value.
    Call { name: String, name_span: Span, local: bool, args: Vec<Expr> },
    MethodCall { recv: Box<Expr>, name: String, name_span: Span, args: Vec<Expr> },
    Field(Box<Expr>, String, Span),
    Index(Box<Expr>, Box<Expr>),
    StructLit { name: String, fields: Vec<(String, Span, Expr)> },
    /// `Enum.Variant(args)`; `Some/None/Ok/Err` use the enum names `Option` / `Result`.
    Ctor { enum_name: String, variant: String, args: Vec<Expr> },
    /// `els` is either a `Block` or another `If` expression.
    If { cond: Box<Expr>, then: Block, els: Option<Box<Expr>> },
    Match { scrutinee: Box<Expr>, arms: Vec<Arm> },
    Block(Block),
    /// `captures` lists the enclosing local variables the body refers to.
    Closure { params: Vec<ClosureParam>, captures: Vec<String>, body: Rc<Expr> },
    Return(Option<Box<Expr>>),
    Break,
    Continue,
    Try(Box<Expr>),
}

pub struct Arm {
    pub pat: Pattern,
    pub body: Expr,
}

pub struct Pattern {
    pub kind: PatKind,
    pub span: Span,
}

pub enum PatKind {
    Wildcard,
    Bind(String),
    Int(i64),
    Str(String),
    Bool(bool),
    Ctor { enum_name: String, variant: String, args: Vec<Pattern> },
}
