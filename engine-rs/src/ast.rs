// AST: expressions and statements produced by the parser.
//
// Extracted from `main.rs` without behavior change; see docs/ENGINE-SPLIT.md.

#[derive(Clone, Debug)]
pub(crate) enum CallArg {
    Pos(Expr),
    Star(Expr),
    Kw(String, Expr),
    KwStar(Expr),
}

#[derive(Clone, Debug)]
#[allow(clippy::enum_variant_names)]
pub(crate) enum Expr {
    Num(f64),
    Str(String),
    Bool(bool),
    Null,
    Var(String),
    List(Vec<Expr>),
    Map(Vec<(Expr, Expr)>),
    Index(Box<Expr>, Box<Expr>),
    Member(Box<Expr>, String),
    MethodCall(Box<Expr>, String, Vec<Expr>),
    Await(Box<Expr>),
    Lambda(Vec<(String, Option<String>)>, Option<String>, Vec<Stmt>),
    ListComp(Box<Expr>, String, Box<Expr>, Option<Box<Expr>>),
    Ternary(Box<Expr>, Box<Expr>, Box<Expr>),
    ChainedCmp(Box<Expr>, Vec<(String, Expr)>),
    Unary(String, Box<Expr>),
    Bin(Box<Expr>, String, Box<Expr>),
    Call(String, Vec<Expr>),
    CallExpr(Box<Expr>, Vec<Expr>),
    CallExt {
        callee: Box<Expr>,
        method: Option<String>,
        args: Vec<CallArg>,
    },
}
#[derive(Clone, Debug)]
pub(crate) enum Stmt {
    Let(String, Option<String>, Expr, bool),
    Assign(String, Expr),
    Expr(Expr),
    If(Expr, Vec<Stmt>, Vec<Stmt>),
    While(Expr, Vec<Stmt>),
    ForRange(String, Expr, Expr, Vec<Stmt>),
    ForEach(String, Expr, Vec<Stmt>),
    Try(Vec<Stmt>, String, Vec<Stmt>, Vec<Stmt>),
    Throw(Expr),
    Fn(
        String,
        Vec<(String, Option<String>)>,
        Option<String>,
        Vec<Stmt>,
    ),
    AsyncFn(
        String,
        Vec<(String, Option<String>)>,
        Option<String>,
        Vec<Stmt>,
    ),
    DecoratedFn(
        Vec<Expr>,
        String,
        Vec<(String, Option<String>)>,
        Option<String>,
        Vec<Stmt>,
    ),
    Class(String, Vec<Expr>, Option<Expr>, Vec<Expr>, Vec<Stmt>),
    SetMember(Expr, String, Expr),
    SetIndex(Expr, Expr, Expr),
    Match(Expr, Vec<(Pattern, Option<Expr>, Vec<Stmt>)>),
    With(Expr, String, Vec<Stmt>),
    Return(Option<Expr>),
    Yield(Expr),
    Break,
    Continue,
    /// Debug-only statement marker carrying 1-based source line number.
    SourceLine(usize),
}

#[derive(Clone, Debug)]
pub(crate) enum SeqElem {
    Pat(Pattern),
    Star(Option<String>),
}

#[derive(Clone, Debug)]
pub(crate) enum Pattern {
    Wildcard,
    Capture(String),
    Literal(Expr),
    Or(Vec<Pattern>),
    As(Box<Pattern>, String),
    Seq(Vec<SeqElem>),
    Map {
        entries: Vec<(Expr, Pattern)>,
        rest: Option<String>,
    },
    Class {
        cls: Expr,
        pos: Vec<Pattern>,
        kw: Vec<(String, Pattern)>,
    },
}
