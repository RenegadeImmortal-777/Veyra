use std::{
    collections::HashMap,
    env, fs,
    io::{self, Write},
    path::PathBuf,
    process::ExitCode,
};

const VERSION: &str = "2.0.0-horizon";

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Num(f64),
    Str(String),
    Id(String),
    Let,
    Const,
    If,
    Else,
    While,
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
                "if" => Tok::If,
                "else" => Tok::Else,
                "while" => Tok::While,
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
                out.push(Tok::Minus);
                i += 1
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
        self.equality()
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
        if self.eat(&Tok::Bang) {
            return Ok(Expr::Unary("!".into(), Box::new(self.unary()?)));
        }
        if self.eat(&Tok::Minus) {
            return Ok(Expr::Unary("-".into(), Box::new(self.unary()?)));
        }
        self.primary()
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
}
impl Val {
    fn truthy(&self) -> bool {
        match self {
            Val::Bool(b) => *b,
            Val::Null => false,
            Val::Num(n) => *n != 0.0,
            Val::Str(s) => !s.is_empty(),
            Val::List(v) => !v.is_empty(),
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
        }
    }
}

#[derive(Clone, Debug)]
enum Op {
    Const(Val),
    Load(String),
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
    Call(String, usize),
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
                // list literal lowered through internal builtin
                for x in items {
                    self.emit_expr(x)?
                }
                self.code.push(Op::Call("__list".into(), items.len()));
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
                let _ = *c;
                self.emit_expr(e)?;
                self.code.push(Op::Store(n.clone()));
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
}
impl Vm {
    fn new(code: Vec<Op>) -> Self {
        Self {
            code,
            ip: 0,
            stack: vec![],
            vars: HashMap::new(),
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
                Op::Store(n) => {
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
                                _ => return Err("len supports String/List".into()),
                            })
                        }
                        "str" => {
                            if a.len() != 1 {
                                return Err("str expects 1 argument".into());
                            }
                            Val::Str(a[0].show())
                        }
                        "__list" => Val::List(a),
                        _ => return Err(format!("unknown function '{n}'")),
                    };
                    self.stack.push(v);
                }
                Op::Halt => break,
            }
        }
        Ok(())
    }
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
        "Veyra {VERSION}\n\nUsage:\n  veyra run <file.vy>\n  veyra check <file.vy>\n  veyra fmt <file.vy>\n  veyra new <name>\n  veyra doctor\n  veyra repl\n  veyra --version"
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
            .and_then(|p| fs::read_to_string(p).map_err(|e| e.to_string()))
            .and_then(|s| compile_source(&s).map(|_| println!("OK"))),
        Some("run") => a
            .get(2)
            .ok_or("missing file".into())
            .and_then(|p| fs::read_to_string(p).map_err(|e| e.to_string()))
            .and_then(|s| run_source(&s)),
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
}
