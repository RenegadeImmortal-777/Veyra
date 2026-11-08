// AST: expressions and statements produced by the parser.
//
// Extracted from `main.rs` without behavior change; see docs/ENGINE-SPLIT.md.

#[derive(Clone, Debug)]
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
    Unary(String, Box<Expr>),
    Bin(Box<Expr>, String, Box<Expr>),
    Call(String, Vec<Expr>),
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
        String,
        String,
        Vec<(String, Option<String>)>,
        Option<String>,
        Vec<Stmt>,
    ),
    Class(String, Option<String>, Vec<Stmt>),
    SetMember(Expr, String, Expr),
    SetIndex(Expr, Expr, Expr),
    Match(Expr, Vec<(Option<Expr>, Vec<Stmt>)>),
    With(Expr, String, Vec<Stmt>),
    Return(Option<Expr>),
    Yield(Expr),
    Break,
    Continue,
}
