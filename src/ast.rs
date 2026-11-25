//! Abstract syntax tree.

use crate::diag::Span;
use std::rc::Rc;

pub struct Program {
    pub items: Vec<Item>,
}

pub enum Item {
    Fn(Rc<FnDecl>),
    Struct(StructDecl),
    Enum(EnumDecl),
    Test(Rc<TestDecl>),
}

pub struct Param {
    pub name: String,
    pub ty: TypeExpr,
    pub span: Span,
}

pub struct FnDecl {
    pub name: String,
    pub name_span: Span,
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
    pub fields: Vec<Param>,
}

pub struct EnumDecl {
    pub name: String,
    pub name_span: Span,
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
    pub effects: Vec<(String, Span)>,
    pub body: Block,
}

pub struct TypeExpr {
    pub name: String,
    pub args: Vec<TypeExpr>,
    pub span: Span,
}

impl std::fmt::Display for TypeExpr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name)?;
        if !self.args.is_empty() {
            write!(f, "<")?;
            for (i, a) in self.args.iter().enumerate() {
                if i > 0 {
                    write!(f, ", ")?;
                }
                write!(f, "{a}")?;
            }
            write!(f, ">")?;
        }
        Ok(())
    }
}

pub struct Block {
    pub stmts: Vec<Stmt>,
    pub span: Span,
}

pub enum Stmt {
    Let { name: String, name_span: Span, mutable: bool, ty: Option<TypeExpr>, value: Expr },
    Assign { target: Expr, value: Expr },
    While { cond: Expr, body: Block },
    /// `for var in iter { .. }` or, when `end` is set, `for var in iter..end { .. }`.
    For { var: String, var_span: Span, iter: Expr, end: Option<Expr>, body: Block },
    Assert { cond: Expr, span: Span },
    Expr(Expr),
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

pub enum ExprKind {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(Vec<StrPart>),
    Ident(String),
    List(Vec<Expr>),
    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Call { name: String, name_span: Span, args: Vec<Expr> },
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

impl Pattern {
    pub fn is_irrefutable(&self) -> bool {
        matches!(self.kind, PatKind::Wildcard | PatKind::Bind(_))
    }
}
