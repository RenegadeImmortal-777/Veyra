use std::{
    collections::{HashMap, HashSet},
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::ExitCode,
};

const VERSION: &str = "3.1.0-horizon";

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Num(f64),
    Str(String),
    Id(String),
    Let,
    Const,
    Fn,
    Return,
    If,
    Else,
    While,
    For,
    In,
    And,
    Or,
    Not,
    True,
    False,
    Null,
    Lp,
    Rp,
    Lb,
    Rb,
    Ls,
    Rs,
    Comma,
    Colon,
    Semi,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Eq,
    EqEq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Bang,
    Arrow,
    Eof,
}

fn lex(src: &str) -> Result<Vec<Tok>, String> {
    let mut out = Vec::new();
    let cs: Vec<char> = src.chars().collect();
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '/' && i + 1 < cs.len() && cs[i + 1] == '/' {
            i += 2;
            while i < cs.len() && cs[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c.is_ascii_digit() {
            let s = i;
            i += 1;
            while i < cs.len() && (cs[i].is_ascii_digit() || cs[i] == '.') {
                i += 1;
            }
            let n: f64 = cs[s..i]
                .iter()
                .collect::<String>()
                .parse()
                .map_err(|_| "bad number")?;
            out.push(Tok::Num(n));
            continue;
        }
        if c == '"' {
            i += 1;
            let mut s = String::new();
            while i < cs.len() && cs[i] != '"' {
                if cs[i] == '\\' && i + 1 < cs.len() {
                    i += 1;
                    s.push(match cs[i] {
                        'n' => '\n',
                        't' => '\t',
                        'r' => '\r',
                        x => x,
                    });
                } else {
                    s.push(cs[i]);
                }
                i += 1;
            }
            if i >= cs.len() {
                return Err("unterminated string".into());
            }
            i += 1;
            out.push(Tok::Str(s));
            continue;
        }
        if c.is_ascii_alphabetic() || c == '_' {
            let s = i;
            i += 1;
            while i < cs.len() && (cs[i].is_ascii_alphanumeric() || cs[i] == '_') {
                i += 1;
            }
            let w: String = cs[s..i].iter().collect();
            out.push(match w.as_str() {
                "let" => Tok::Let,
                "const" => Tok::Const,
                "fn" => Tok::Fn,
                "return" => Tok::Return,
                "if" => Tok::If,
                "else" => Tok::Else,
                "while" => Tok::While,
                "for" => Tok::For,
                "in" => Tok::In,
                "and" => Tok::And,
                "or" => Tok::Or,
                "not" => Tok::Not,
                "true" => Tok::True,
                "false" => Tok::False,
                "null" => Tok::Null,
                _ => Tok::Id(w),
            });
            continue;
        }
        macro_rules! two {
            ($ch:expr,$eq:expr,$one:expr) => {{
                if i + 1 < cs.len() && cs[i + 1] == '=' {
                    i += 2;
                    out.push($eq);
                } else {
                    i += 1;
                    out.push($one);
                }
                continue;
            }};
        }
        match c {
            '(' => {
                out.push(Tok::Lp);
                i += 1
            }
            ')' => {
                out.push(Tok::Rp);
                i += 1
            }
            '{' => {
                out.push(Tok::Lb);
                i += 1
            }
            '}' => {
                out.push(Tok::Rb);
                i += 1
            }
            '[' => {
                out.push(Tok::Ls);
                i += 1
            }
            ']' => {
                out.push(Tok::Rs);
                i += 1
            }
            ',' => {
                out.push(Tok::Comma);
                i += 1
            }
            ':' => {
                out.push(Tok::Colon);
                i += 1
            }
            ';' => {
                out.push(Tok::Semi);
                i += 1
            }
            '+' => {
                out.push(Tok::Plus);
                i += 1
            }
            '-' => {
                if i + 1 < cs.len() && cs[i + 1] == '>' {
                    out.push(Tok::Arrow);
                    i += 2
                } else {
                    out.push(Tok::Minus);
                    i += 1
                }
            }
            '*' => {
                out.push(Tok::Star);
                i += 1
            }
            '/' => {
                out.push(Tok::Slash);
                i += 1
            }
            '%' => {
                out.push(Tok::Percent);
                i += 1
            }
            '=' => two!('=', Tok::EqEq, Tok::Eq),
            '!' => two!('=', Tok::Ne, Tok::Bang),
            '<' => two!('=', Tok::Le, Tok::Lt),
            '>' => two!('=', Tok::Ge, Tok::Gt),
            _ => return Err(format!("unexpected character {c:?}")),
        }
    }
    out.push(Tok::Eof);
    Ok(out)
}

#[derive(Clone, Debug)]
enum Expr {
    Num(f64),
    Str(String),
    Bool(bool),
    Null,
    Var(String),
    List(Vec<Expr>),
    Map(Vec<(Expr, Expr)>),
    Index(Box<Expr>, Box<Expr>),
    Unary(String, Box<Expr>),
    Bin(Box<Expr>, String, Box<Expr>),
    Call(String, Vec<Expr>),
}
#[derive(Clone, Debug)]
enum Stmt {
    Let(String, Expr, bool),
    Assign(String, Expr),
    Expr(Expr),
    If(Expr, Vec<Stmt>, Vec<Stmt>),
    While(Expr, Vec<Stmt>),
    ForRange(String, Expr, Expr, Vec<Stmt>),
    Fn(String, Vec<String>, Vec<Stmt>),
    Return(Option<Expr>),
}

struct Parser {
    t: Vec<Tok>,
    i: usize,
}
impl Parser {
    fn new(t: Vec<Tok>) -> Self {
        Self { t, i: 0 }
    }
    fn cur(&self) -> &Tok {
        &self.t[self.i]
    }
    fn bump(&mut self) -> Tok {
        let x = self.t[self.i].clone();
        self.i += 1;
        x
    }
    fn same(a: &Tok, b: &Tok) -> bool {
        std::mem::discriminant(a) == std::mem::discriminant(b)
    }
    fn eat(&mut self, w: &Tok) -> bool {
        if Self::same(self.cur(), w) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn need(&mut self, w: &Tok, msg: &str) -> Result<(), String> {
        if self.eat(w) {
            Ok(())
        } else {
            Err(format!("{msg}; got {:?}", self.cur()))
        }
    }
    fn program(&mut self) -> Result<Vec<Stmt>, String> {
        let mut v = vec![];
        while !matches!(self.cur(), Tok::Eof) {
            v.push(self.stmt()?);
        }
        Ok(v)
    }
    fn block(&mut self) -> Result<Vec<Stmt>, String> {
        self.need(&Tok::Lb, "expected '{'")?;
        let mut v = vec![];
        while !matches!(self.cur(), Tok::Rb | Tok::Eof) {
            v.push(self.stmt()?);
        }
        self.need(&Tok::Rb, "expected '}'")?;
        Ok(v)
    }
    fn stmt(&mut self) -> Result<Stmt, String> {
        if self.eat(&Tok::Let) || self.eat(&Tok::Const) {
            let is_const = matches!(self.t[self.i - 1], Tok::Const);
            let name = match self.bump() {
                Tok::Id(s) => s,
                x => return Err(format!("expected name, got {x:?}")),
            };
            if self.eat(&Tok::Colon) {
                if !matches!(self.bump(), Tok::Id(_)) {
                    return Err("expected type name".into());
                }
            }
            self.need(&Tok::Eq, "expected '='")?;
            let e = self.expr()?;
            self.eat(&Tok::Semi);
            return Ok(Stmt::Let(name, e, is_const));
        }
        if self.eat(&Tok::Fn) {
            let name = match self.bump() {
                Tok::Id(s) => s,
                x => return Err(format!("expected function name, got {x:?}")),
            };
            self.need(&Tok::Lp, "expected '(' after function name")?;
            let mut params = vec![];
            if !self.eat(&Tok::Rp) {
                loop {
                    let param = match self.bump() {
                        Tok::Id(s) => s,
                        x => return Err(format!("expected parameter name, got {x:?}")),
                    };
                    if self.eat(&Tok::Colon) {
                        if !matches!(self.bump(), Tok::Id(_)) {
                            return Err("expected parameter type".into());
                        }
                    }
                    params.push(param);
                    if self.eat(&Tok::Rp) {
                        break;
                    }
                    self.need(&Tok::Comma, "expected ',' between parameters")?;
                }
            }
            if self.eat(&Tok::Arrow) {
                if !matches!(self.bump(), Tok::Id(_)) {
                    return Err("expected return type".into());
                }
            }
            let body = self.block()?;
            return Ok(Stmt::Fn(name, params, body));
        }
        if self.eat(&Tok::Return) {
            if matches!(self.cur(), Tok::Semi | Tok::Rb) {
                self.eat(&Tok::Semi);
                return Ok(Stmt::Return(None));
            }
            let value = self.expr()?;
            self.eat(&Tok::Semi);
            return Ok(Stmt::Return(Some(value)));
        }
        if self.eat(&Tok::If) {
            let c = self.expr()?;
            let a = self.block()?;
            let b = if self.eat(&Tok::Else) {
                self.block()?
            } else {
                vec![]
            };
            return Ok(Stmt::If(c, a, b));
        }
        if self.eat(&Tok::While) {
            let c = self.expr()?;
            let b = self.block()?;
            return Ok(Stmt::While(c, b));
        }
        if self.eat(&Tok::For) {
            let name = match self.bump() {
                Tok::Id(s) => s,
                x => return Err(format!("expected loop variable, got {x:?}")),
            };
            self.need(&Tok::In, "expected 'in'")?;
            let range_name = match self.bump() {
                Tok::Id(s) if s == "range" => s,
                x => {
                    return Err(format!(
                        "native for currently expects range(...), got {x:?}"
                    ));
                }
            };
            let _ = range_name;
            self.need(&Tok::Lp, "expected '(' after range")?;
            let start = self.expr()?;
            self.need(&Tok::Comma, "expected ',' in range")?;
            let end = self.expr()?;
            self.need(&Tok::Rp, "expected ')' after range")?;
            let body = self.block()?;
            return Ok(Stmt::ForRange(name, start, end, body));
        }
        if let Tok::Id(n) = self.cur().clone() {
            if self.i + 1 < self.t.len() && matches!(self.t[self.i + 1], Tok::Eq) {
                self.i += 2;
                let e = self.expr()?;
                self.eat(&Tok::Semi);
                return Ok(Stmt::Assign(n, e));
            }
        }
        let e = self.expr()?;
        self.eat(&Tok::Semi);
        Ok(Stmt::Expr(e))
    }
    fn expr(&mut self) -> Result<Expr, String> {
        self.logic_or()
    }
    fn logic_or(&mut self) -> Result<Expr, String> {
        let mut e = self.logic_and()?;
        while self.eat(&Tok::Or) {
            e = Expr::Bin(Box::new(e), "or".into(), Box::new(self.logic_and()?));
        }
        Ok(e)
    }
    fn logic_and(&mut self) -> Result<Expr, String> {
        let mut e = self.equality()?;
        while self.eat(&Tok::And) {
            e = Expr::Bin(Box::new(e), "and".into(), Box::new(self.equality()?));
        }
        Ok(e)
    }
    fn equality(&mut self) -> Result<Expr, String> {
        let mut e = self.compare()?;
        loop {
            let op = match self.cur() {
                Tok::EqEq => "==",
                Tok::Ne => "!=",
                _ => break,
            }
            .to_string();
            self.i += 1;
            e = Expr::Bin(Box::new(e), op, Box::new(self.compare()?));
        }
        Ok(e)
    }
    fn compare(&mut self) -> Result<Expr, String> {
        let mut e = self.term()?;
        loop {
            let op = match self.cur() {
                Tok::Lt => "<",
                Tok::Le => "<=",
                Tok::Gt => ">",
                Tok::Ge => ">=",
                _ => break,
            }
            .to_string();
            self.i += 1;
            e = Expr::Bin(Box::new(e), op, Box::new(self.term()?));
        }
        Ok(e)
    }
    fn term(&mut self) -> Result<Expr, String> {
        let mut e = self.factor()?;
        loop {
            let op = match self.cur() {
                Tok::Plus => "+",
                Tok::Minus => "-",
                _ => break,
            }
            .to_string();
            self.i += 1;
            e = Expr::Bin(Box::new(e), op, Box::new(self.factor()?));
        }
        Ok(e)
    }
    fn factor(&mut self) -> Result<Expr, String> {
        let mut e = self.unary()?;
        loop {
            let op = match self.cur() {
                Tok::Star => "*",
                Tok::Slash => "/",
                Tok::Percent => "%",
                _ => break,
            }
            .to_string();
            self.i += 1;
            e = Expr::Bin(Box::new(e), op, Box::new(self.unary()?));
        }
        Ok(e)
    }
    fn unary(&mut self) -> Result<Expr, String> {
        if self.eat(&Tok::Bang) || self.eat(&Tok::Not) {
            return Ok(Expr::Unary("!".into(), Box::new(self.unary()?)));
        }
        if self.eat(&Tok::Minus) {
            return Ok(Expr::Unary("-".into(), Box::new(self.unary()?)));
        }
        self.postfix()
    }
    fn postfix(&mut self) -> Result<Expr, String> {
        let mut e = self.primary()?;
        while self.eat(&Tok::Ls) {
            let key = self.expr()?;
            self.need(&Tok::Rs, "expected ']' after index")?;
            e = Expr::Index(Box::new(e), Box::new(key));
        }
        Ok(e)
    }
    fn primary(&mut self) -> Result<Expr, String> {
        match self.bump() {
            Tok::Num(n) => Ok(Expr::Num(n)),
            Tok::Str(s) => Ok(Expr::Str(s)),
            Tok::True => Ok(Expr::Bool(true)),
            Tok::False => Ok(Expr::Bool(false)),
            Tok::Null => Ok(Expr::Null),
            Tok::Id(n) => {
                if self.eat(&Tok::Lp) {
                    let mut a = vec![];
                    if !self.eat(&Tok::Rp) {
                        loop {
                            a.push(self.expr()?);
                            if self.eat(&Tok::Rp) {
                                break;
                            }
                            self.need(&Tok::Comma, "expected ','")?;
                        }
                    }
                    Ok(Expr::Call(n, a))
                } else {
                    Ok(Expr::Var(n))
                }
            }
            Tok::Lp => {
                let e = self.expr()?;
                self.need(&Tok::Rp, "expected ')'")?;
                Ok(e)
            }
            Tok::Ls => {
                let mut a = vec![];
                if !self.eat(&Tok::Rs) {
                    loop {
                        a.push(self.expr()?);
                        if self.eat(&Tok::Rs) {
                            break;
                        }
                        self.need(&Tok::Comma, "expected ','")?;
                    }
                }
                Ok(Expr::List(a))
            }
            Tok::Lb => {
                let mut pairs = vec![];
                if !self.eat(&Tok::Rb) {
                    loop {
                        let key = self.expr()?;
                        self.need(&Tok::Colon, "expected ':' in map")?;
                        let value = self.expr()?;
                        pairs.push((key, value));
                        if self.eat(&Tok::Rb) {
                            break;
                        }
                        self.need(&Tok::Comma, "expected ',' in map")?;
                    }
                }
                Ok(Expr::Map(pairs))
            }
            x => Err(format!("expected expression, got {x:?}")),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Val {
    Num(f64),
    Str(String),
    Bool(bool),
    Null,
    List(Vec<Val>),
    Map(Vec<(Val, Val)>),
}
impl Val {
    fn truthy(&self) -> bool {
        match self {
            Val::Bool(b) => *b,
            Val::Null => false,
            Val::Num(n) => *n != 0.0,
            Val::Str(s) => !s.is_empty(),
            Val::List(v) => !v.is_empty(),
            Val::Map(v) => !v.is_empty(),
        }
    }
    fn show(&self) -> String {
        match self {
            Val::Num(n) => {
                if n.fract() == 0.0 {
                    format!("{}", *n as i64)
                } else {
                    format!("{n}")
                }
            }
            Val::Str(s) => s.clone(),
            Val::Bool(b) => b.to_string(),
            Val::Null => "null".into(),
            Val::List(v) => format!(
                "[{}]",
                v.iter().map(Val::show).collect::<Vec<_>>().join(", ")
            ),
            Val::Map(v) => format!(
                "{{{}}}",
                v.iter()
                    .map(|(k, x)| format!("{}: {}", k.show(), x.show()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

#[derive(Clone, Debug)]
enum Op {
    Const(Val),
    Load(String),
    Define(String, bool),
    Store(String),
    Pop,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Neg,
    Not,
    And,
    Or,
    Index,
    Call(String, usize),
    Def(String, Vec<String>, Vec<Op>),
    Return,
    JumpIfFalse(usize),
    Jump(usize),
    Halt,
}

struct Compiler {
    code: Vec<Op>,
}
impl Compiler {
    fn new() -> Self {
        Self { code: vec![] }
    }
    fn emit_expr(&mut self, e: &Expr) -> Result<(), String> {
        match e {
            Expr::Num(n) => self.code.push(Op::Const(Val::Num(*n))),
            Expr::Str(s) => self.code.push(Op::Const(Val::Str(s.clone()))),
            Expr::Bool(b) => self.code.push(Op::Const(Val::Bool(*b))),
            Expr::Null => self.code.push(Op::Const(Val::Null)),
            Expr::List(items) => {
                for x in items {
                    self.emit_expr(x)?
                }
                self.code.push(Op::Call("__list".into(), items.len()));
            }
            Expr::Map(pairs) => {
                for (k, v) in pairs {
                    self.emit_expr(k)?;
                    self.emit_expr(v)?;
                }
                self.code.push(Op::Call("__map".into(), pairs.len() * 2));
            }
            Expr::Index(obj, key) => {
                self.emit_expr(obj)?;
                self.emit_expr(key)?;
                self.code.push(Op::Index);
            }
            Expr::Var(n) => self.code.push(Op::Load(n.clone())),
            Expr::Unary(op, x) => {
                self.emit_expr(x)?;
                self.code.push(if op == "-" { Op::Neg } else { Op::Not });
            }
            Expr::Bin(a, op, b) => {
                self.emit_expr(a)?;
                self.emit_expr(b)?;
                self.code.push(match op.as_str() {
                    "+" => Op::Add,
                    "-" => Op::Sub,
                    "*" => Op::Mul,
                    "/" => Op::Div,
                    "%" => Op::Mod,
                    "==" => Op::Eq,
                    "!=" => Op::Ne,
                    "<" => Op::Lt,
                    "<=" => Op::Le,
                    ">" => Op::Gt,
                    ">=" => Op::Ge,
                    "and" => Op::And,
                    "or" => Op::Or,
                    _ => return Err("bad operator".into()),
                });
            }
            Expr::Call(n, args) => {
                for a in args {
                    self.emit_expr(a)?
                }
                self.code.push(Op::Call(n.clone(), args.len()));
            }
        }
        Ok(())
    }
    fn emit_stmts(&mut self, ss: &[Stmt]) -> Result<(), String> {
        for s in ss {
            self.emit_stmt(s)?
        }
        Ok(())
    }
    fn emit_stmt(&mut self, s: &Stmt) -> Result<(), String> {
        match s {
            Stmt::Let(n, e, c) => {
                self.emit_expr(e)?;
                self.code.push(Op::Define(n.clone(), *c));
            }
            Stmt::Assign(n, e) => {
                self.emit_expr(e)?;
                self.code.push(Op::Store(n.clone()));
            }
            Stmt::Expr(e) => {
                self.emit_expr(e)?;
                self.code.push(Op::Pop);
            }
            Stmt::If(c, a, b) => {
                self.emit_expr(c)?;
                let jf = self.code.len();
                self.code.push(Op::JumpIfFalse(0));
                self.emit_stmts(a)?;
                let j = self.code.len();
                self.code.push(Op::Jump(0));
                let else_pc = self.code.len();
                self.emit_stmts(b)?;
                let end = self.code.len();
                self.code[jf] = Op::JumpIfFalse(else_pc);
                self.code[j] = Op::Jump(end);
            }
            Stmt::While(c, b) => {
                let start = self.code.len();
                self.emit_expr(c)?;
                let jf = self.code.len();
                self.code.push(Op::JumpIfFalse(0));
                self.emit_stmts(b)?;
                self.code.push(Op::Jump(start));
                let end = self.code.len();
                self.code[jf] = Op::JumpIfFalse(end);
            }
            Stmt::ForRange(name, start_expr, end_expr, body) => {
                self.emit_expr(start_expr)?;
                self.code.push(Op::Store(name.clone()));
                let loop_start = self.code.len();
                self.code.push(Op::Load(name.clone()));
                self.emit_expr(end_expr)?;
                self.code.push(Op::Lt);
                let jf = self.code.len();
                self.code.push(Op::JumpIfFalse(0));
                self.emit_stmts(body)?;
                self.code.push(Op::Load(name.clone()));
                self.code.push(Op::Const(Val::Num(1.0)));
                self.code.push(Op::Add);
                self.code.push(Op::Store(name.clone()));
                self.code.push(Op::Jump(loop_start));
                let end = self.code.len();
                self.code[jf] = Op::JumpIfFalse(end);
            }
            Stmt::Fn(name, params, body) => {
                let mut child = Compiler::new();
                child.emit_stmts(body)?;
                child.code.push(Op::Const(Val::Null));
                child.code.push(Op::Return);
                self.code
                    .push(Op::Def(name.clone(), params.clone(), child.code));
            }
            Stmt::Return(value) => {
                if let Some(expr) = value {
                    self.emit_expr(expr)?;
                } else {
                    self.code.push(Op::Const(Val::Null));
                }
                self.code.push(Op::Return);
            }
        }
        Ok(())
    }
    fn finish(mut self) -> Vec<Op> {
        self.code.push(Op::Halt);
        self.code
    }
}

struct Vm {
    code: Vec<Op>,
    ip: usize,
    stack: Vec<Val>,
    vars: HashMap<String, Val>,
    consts: HashSet<String>,
    funcs: HashMap<String, (Vec<String>, Vec<Op>)>,
}
impl Vm {
    fn new(code: Vec<Op>) -> Self {
        Self {
            code,
            ip: 0,
            stack: vec![],
            vars: HashMap::new(),
            consts: HashSet::new(),
            funcs: HashMap::new(),
        }
    }
    fn nested(
        code: Vec<Op>,
        vars: HashMap<String, Val>,
        funcs: HashMap<String, (Vec<String>, Vec<Op>)>,
    ) -> Self {
        Self {
            code,
            ip: 0,
            stack: vec![],
            vars,
            consts: HashSet::new(),
            funcs,
        }
    }
    fn pop(&mut self) -> Result<Val, String> {
        self.stack.pop().ok_or("stack underflow".into())
    }
    fn bin_num<F: Fn(f64, f64) -> f64>(&mut self, f: F) -> Result<(), String> {
        let b = self.pop()?;
        let a = self.pop()?;
        if let (Val::Num(x), Val::Num(y)) = (a, b) {
            self.stack.push(Val::Num(f(x, y)));
            Ok(())
        } else {
            Err("numeric operands required".into())
        }
    }
    fn run(&mut self) -> Result<(), String> {
        self.execute().map(|_| ())
    }
    fn execute(&mut self) -> Result<Val, String> {
        loop {
            let op = self
                .code
                .get(self.ip)
                .cloned()
                .ok_or("instruction pointer out of range")?;
            self.ip += 1;
            match op {
                Op::Const(v) => self.stack.push(v),
                Op::Load(n) => self.stack.push(
                    self.vars
                        .get(&n)
                        .cloned()
                        .ok_or(format!("undefined name '{n}'"))?,
                ),
                Op::Define(n, is_const) => {
                    let v = self.pop()?;
                    if self.vars.contains_key(&n) {
                        return Err(format!("name '{n}' already defined"));
                    }
                    self.vars.insert(n.clone(), v);
                    if is_const {
                        self.consts.insert(n);
                    }
                }
                Op::Store(n) => {
                    if self.consts.contains(&n) {
                        return Err(format!("cannot assign to constant '{n}'"));
                    }
                    let v = self.pop()?;
                    self.vars.insert(n, v);
                }
                Op::Pop => {
                    let _ = self.pop()?;
                }
                Op::Add => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.stack.push(match (a, b) {
                        (Val::Num(x), Val::Num(y)) => Val::Num(x + y),
                        (Val::Str(x), Val::Str(y)) => Val::Str(x + &y),
                        _ => return Err("invalid '+' operands".into()),
                    });
                }
                Op::Sub => self.bin_num(|a, b| a - b)?,
                Op::Mul => self.bin_num(|a, b| a * b)?,
                Op::Div => self.bin_num(|a, b| a / b)?,
                Op::Mod => self.bin_num(|a, b| a % b)?,
                Op::Eq => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.stack.push(Val::Bool(a == b));
                }
                Op::Ne => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.stack.push(Val::Bool(a != b));
                }
                Op::Lt | Op::Le | Op::Gt | Op::Ge => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    let (x, y) = match (a, b) {
                        (Val::Num(x), Val::Num(y)) => (x, y),
                        _ => return Err("comparison requires numbers".into()),
                    };
                    let yes = match op {
                        Op::Lt => x < y,
                        Op::Le => x <= y,
                        Op::Gt => x > y,
                        _ => x >= y,
                    };
                    self.stack.push(Val::Bool(yes));
                }
                Op::Neg => {
                    let a = self.pop()?;
                    if let Val::Num(x) = a {
                        self.stack.push(Val::Num(-x))
                    } else {
                        return Err("unary '-' requires number".into());
                    }
                }
                Op::Not => {
                    let a = self.pop()?;
                    self.stack.push(Val::Bool(!a.truthy()));
                }
                Op::And => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.stack.push(Val::Bool(a.truthy() && b.truthy()));
                }
                Op::Or => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.stack.push(Val::Bool(a.truthy() || b.truthy()));
                }
                Op::Index => {
                    let key = self.pop()?;
                    let obj = self.pop()?;
                    let value = match (obj, key) {
                        (Val::List(v), Val::Num(n)) => {
                            let i = n as usize;
                            v.get(i)
                                .cloned()
                                .ok_or(format!("list index {i} out of range"))?
                        }
                        (Val::Str(s), Val::Num(n)) => {
                            let i = n as usize;
                            s.chars()
                                .nth(i)
                                .map(|c| Val::Str(c.to_string()))
                                .ok_or(format!("string index {i} out of range"))?
                        }
                        (Val::Map(pairs), k) => pairs
                            .iter()
                            .find(|(key, _)| *key == k)
                            .map(|(_, v)| v.clone())
                            .ok_or("map key not found")?,
                        _ => return Err("value is not indexable".into()),
                    };
                    self.stack.push(value);
                }
                Op::JumpIfFalse(p) => {
                    let a = self.pop()?;
                    if !a.truthy() {
                        self.ip = p
                    }
                }
                Op::Jump(p) => self.ip = p,
                Op::Call(n, k) => {
                    let mut a = vec![];
                    for _ in 0..k {
                        a.push(self.pop()?)
                    }
                    a.reverse();
                    let v = match n.as_str() {
                        "print" => {
                            println!("{}", a.iter().map(Val::show).collect::<Vec<_>>().join(" "));
                            Val::Null
                        }
                        "len" => {
                            if a.len() != 1 {
                                return Err("len expects 1 argument".into());
                            }
                            Val::Num(match &a[0] {
                                Val::Str(s) => s.chars().count() as f64,
                                Val::List(v) => v.len() as f64,
                                Val::Map(v) => v.len() as f64,
                                _ => return Err("len supports String/List/Map".into()),
                            })
                        }
                        "str" => {
                            if a.len() != 1 {
                                return Err("str expects 1 argument".into());
                            }
                            Val::Str(a[0].show())
                        }
                        "__list" => Val::List(a),
                        "__map" => {
                            if a.len() % 2 != 0 {
                                return Err("internal map arity error".into());
                            }
                            let mut pairs = vec![];
                            let mut it = a.into_iter();
                            while let Some(k) = it.next() {
                                let v = it.next().ok_or("internal map value missing")?;
                                pairs.push((k, v));
                            }
                            Val::Map(pairs)
                        }
                        "get" => {
                            if a.len() != 2 {
                                return Err("get expects map and key".into());
                            }
                            match &a[0] {
                                Val::Map(pairs) => pairs
                                    .iter()
                                    .find(|(k, _)| *k == a[1])
                                    .map(|(_, v)| v.clone())
                                    .unwrap_or(Val::Null),
                                _ => return Err("get expects a map".into()),
                            }
                        }
                        "keys" => {
                            if a.len() != 1 {
                                return Err("keys expects a map".into());
                            }
                            match &a[0] {
                                Val::Map(pairs) => {
                                    Val::List(pairs.iter().map(|(k, _)| k.clone()).collect())
                                }
                                _ => return Err("keys expects a map".into()),
                            }
                        }
                        "values" => {
                            if a.len() != 1 {
                                return Err("values expects a map".into());
                            }
                            match &a[0] {
                                Val::Map(pairs) => {
                                    Val::List(pairs.iter().map(|(_, v)| v.clone()).collect())
                                }
                                _ => return Err("values expects a map".into()),
                            }
                        }
                        _ => {
                            let (params, code) = self
                                .funcs
                                .get(&n)
                                .cloned()
                                .ok_or(format!("unknown function '{n}'"))?;
                            if params.len() != a.len() {
                                return Err(format!(
                                    "{n} expects {} arguments, got {}",
                                    params.len(),
                                    a.len()
                                ));
                            }
                            let mut vars = self.vars.clone();
                            for (param, value) in params.into_iter().zip(a.into_iter()) {
                                vars.insert(param, value);
                            }
                            let mut nested = Vm::nested(code, vars, self.funcs.clone());
                            nested.execute()?
                        }
                    };
                    self.stack.push(v);
                }
                Op::Def(name, params, code) => {
                    self.funcs.insert(name, (params, code));
                }
                Op::Return => return self.pop(),
                Op::Halt => return Ok(Val::Null),
            }
        }
    }
}

fn load_source(path: &str) -> Result<String, String> {
    fn load(path: &Path, seen: &mut HashSet<PathBuf>) -> Result<String, String> {
        let full = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if !seen.insert(full.clone()) {
            return Ok(String::new());
        }
        let src = fs::read_to_string(&full).map_err(|e| e.to_string())?;
        let base = full.parent().unwrap_or(Path::new("."));
        let mut out = String::new();
        for line in src.lines() {
            let t = line.trim();
            if t.starts_with("use ") {
                if let Some(a) = t.find('"') {
                    if let Some(b_rel) = t[a + 1..].find('"') {
                        let b = a + 1 + b_rel;
                        let rel = &t[a + 1..b];
                        out.push_str(&load(&base.join(rel), seen)?);
                        out.push('\n');
                        continue;
                    }
                }
            }
            out.push_str(line);
            out.push('\n');
        }
        Ok(out)
    }
    load(Path::new(path), &mut HashSet::new())
}

fn compile_source(src: &str) -> Result<Vec<Op>, String> {
    let t = lex(src)?;
    let ast = Parser::new(t).program()?;
    let mut c = Compiler::new();
    c.emit_stmts(&ast)?;
    Ok(c.finish())
}
fn run_source(src: &str) -> Result<(), String> {
    Vm::new(compile_source(src)?).run()
}

fn disasm_source(src: &str) -> Result<(), String> {
    let code = compile_source(src)?;
    for (i, op) in code.iter().enumerate() {
        println!("{i:04}  {op:?}");
    }
    Ok(())
}

fn fmt_source(src: &str) -> String {
    let mut out = String::new();
    let mut indent = 0usize;
    for raw in src.lines() {
        let line = raw.trim().to_string();
        if line.starts_with('}') {
            indent = indent.saturating_sub(1);
        }
        if !line.is_empty() {
            out.push_str(&"    ".repeat(indent));
            out.push_str(&line);
            out.push('\n');
        }
        if line.ends_with('{') {
            indent += 1;
        }
    }
    out
}

fn new_project(name: &str) -> Result<(), String> {
    let p = PathBuf::from(name);
    if p.exists() {
        return Err(format!("'{name}' already exists"));
    }
    fs::create_dir_all(p.join("src")).map_err(|e| e.to_string())?;
    fs::write(
        p.join("veyra.toml"),
        format!("name = \"{name}\"\nversion = \"0.1.0\"\nentry = \"src/main.vy\"\n"),
    )
    .map_err(|e| e.to_string())?;
    fs::write(
        p.join("src/main.vy"),
        "const language: String = \"Veyra\";\nprint(\"Hello from\", language);\n",
    )
    .map_err(|e| e.to_string())?;
    println!("Created Veyra project {name}");
    Ok(())
}

fn doctor() {
    println!("Veyra {VERSION}");
    println!("engine: native Rust bytecode VM");
    println!("target: {}", env::consts::ARCH);
    println!("os: {}", env::consts::OS);
    println!("status: OK");
}

fn repl() -> Result<(), String> {
    println!("Veyra {VERSION} native REPL â€” :quit to exit");
    let mut line = String::new();
    loop {
        print!("vy> ");
        io::stdout().flush().map_err(|e| e.to_string())?;
        line.clear();
        if io::stdin()
            .read_line(&mut line)
            .map_err(|e| e.to_string())?
            == 0
        {
            break;
        }
        if line.trim() == ":quit" {
            break;
        }
        if let Err(e) = run_source(&line) {
            eprintln!("error: {e}")
        }
    }
    Ok(())
}

fn usage() {
    println!(
        "Veyra {VERSION}\n\nUsage:\n  veyra run <file.vy>\n  veyra check <file.vy>\n  veyra disasm <file.vy>\n  veyra fmt <file.vy>\n  veyra new <name>\n  veyra doctor\n  veyra repl\n  veyra --version"
    );
}
fn main() -> ExitCode {
    let a: Vec<String> = env::args().collect();
    let r: Result<(), String> = match a.get(1).map(String::as_str) {
        Some("--version") | Some("version") => {
            println!("Veyra {VERSION}");
            Ok(())
        }
        Some("doctor") => {
            doctor();
            Ok(())
        }
        Some("repl") | None => repl(),
        Some("new") => a
            .get(2)
            .ok_or("missing project name".into())
            .and_then(|n| new_project(n)),
        Some("check") => a
            .get(2)
            .ok_or("missing file".into())
            .and_then(|p| load_source(p))
            .and_then(|s| compile_source(&s).map(|_| println!("OK"))),
        Some("run") => a
            .get(2)
            .ok_or("missing file".into())
            .and_then(|p| load_source(p))
            .and_then(|s| run_source(&s)),
        Some("disasm") => a
            .get(2)
            .ok_or("missing file".into())
            .and_then(|p| load_source(p))
            .and_then(|s| disasm_source(&s)),
        Some("fmt") => a.get(2).ok_or("missing file".into()).and_then(|p| {
            let s = fs::read_to_string(p).map_err(|e| e.to_string())?;
            fs::write(p, fmt_source(&s)).map_err(|e| e.to_string())
        }),
        Some(x) if x.ends_with(".vy") => fs::read_to_string(x)
            .map_err(|e| e.to_string())
            .and_then(|s| run_source(&s)),
        _ => {
            usage();
            Ok(())
        }
    };
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("veyra: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lex_math() {
        assert!(lex("let x=1+2;").is_ok());
    }
    #[test]
    fn parse_control() {
        assert!(compile_source("let x=0; while x<3 { x=x+1; }").is_ok());
    }
    #[test]
    fn vm_math() {
        assert!(run_source("let x=2+3*4;").is_ok());
    }
    #[test]
    fn list_builtin() {
        assert!(run_source("let x=[1,2,3]; let n=len(x);").is_ok());
    }
    #[test]
    fn for_range_loop() {
        assert!(run_source("let s=0; for n in range(0,5) { s=s+n; }").is_ok());
    }
    #[test]
    fn user_function() {
        assert!(
            run_source("fn add(a: Int, b: Int) -> Int { return a+b; } let x=add(2,3);").is_ok()
        );
    }
    #[test]
    fn recursive_function() {
        assert!(run_source("fn fib(n: Int) -> Int { if n<=1 { return n; } return fib(n-1)+fib(n-2); } let x=fib(8);").is_ok());
    }
    #[test]
    fn map_builtin() {
        assert!(run_source("let m={\"a\":1,\"b\":2}; let n=len(m); let x=get(m,\"b\");").is_ok());
    }
    #[test]
    fn native_indexing() {
        assert!(
            run_source("let xs=[10,20,30]; let x=xs[1]; let m={\"a\":7}; let y=m[\"a\"];").is_ok()
        );
    }
    #[test]
    fn const_is_enforced() {
        assert!(run_source("const x=1; x=2;").is_err());
    }
    #[test]
    fn boolean_logic() {
        assert!(run_source("let x=true and not false; let y=false or x;").is_ok());
    }
}
