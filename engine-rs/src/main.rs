use std::{
    collections::{HashMap, HashSet},
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command, ExitCode},
    time::{SystemTime, UNIX_EPOCH},
};

use rand::Rng;
use regex::Regex;
use rusqlite::Connection;
use serde_json::Value as JsonValue;
use sha2::{Digest, Sha256};
use tiny_http::{Header, Response, Server, StatusCode};

const VERSION: &str = "5.0.0-frontier";

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
fn val_to_json(v: &Val) -> JsonValue {
    match v {
        Val::Num(n) => JsonValue::from(*n),
        Val::Str(s) => JsonValue::from(s.clone()),
        Val::Bool(b) => JsonValue::from(*b),
        Val::Null => JsonValue::Null,
        Val::List(xs) => JsonValue::Array(xs.iter().map(val_to_json).collect()),
        Val::Map(pairs) => {
            let mut obj = serde_json::Map::new();
            for (k, v) in pairs {
                obj.insert(k.show(), val_to_json(v));
            }
            JsonValue::Object(obj)
        }
    }
}

fn json_to_val(v: &JsonValue) -> Val {
    match v {
        JsonValue::Null => Val::Null,
        JsonValue::Bool(b) => Val::Bool(*b),
        JsonValue::Number(n) => Val::Num(n.as_f64().unwrap_or(0.0)),
        JsonValue::String(s) => Val::Str(s.clone()),
        JsonValue::Array(xs) => Val::List(xs.iter().map(json_to_val).collect()),
        JsonValue::Object(obj) => Val::Map(
            obj.iter()
                .map(|(k, v)| (Val::Str(k.clone()), json_to_val(v)))
                .collect(),
        ),
    }
}

fn as_num_list(v: &Val) -> Result<Vec<f64>, String> {
    match v {
        Val::List(xs) => xs
            .iter()
            .map(|x| match x {
                Val::Num(n) => Ok(*n),
                _ => Err("expected numeric list".into()),
            })
            .collect(),
        _ => Err("expected List".into()),
    }
}

fn as_matrix(v: &Val) -> Result<Vec<Vec<f64>>, String> {
    match v {
        Val::List(rows) => rows.iter().map(as_num_list).collect(),
        _ => Err("expected matrix as List<List<Number>>".into()),
    }
}

fn nums_to_val(xs: Vec<f64>) -> Val {
    Val::List(xs.into_iter().map(Val::Num).collect())
}

fn matrix_to_val(xs: Vec<Vec<f64>>) -> Val {
    Val::List(xs.into_iter().map(nums_to_val).collect())
}

fn distance_sq(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b.iter()).map(|(x, y)| (x - y) * (x - y)).sum()
}

fn map_lookup<'a>(pairs: &'a [(Val, Val)], key: &Val) -> Option<&'a Val> {
    pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

fn svg_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
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
                        "json_stringify" => {
                            if a.len() != 1 {
                                return Err("json_stringify expects 1 argument".into());
                            }
                            Val::Str(
                                serde_json::to_string(&val_to_json(&a[0]))
                                    .map_err(|e| e.to_string())?,
                            )
                        }
                        "json_parse" => {
                            if a.len() != 1 {
                                return Err("json_parse expects 1 argument".into());
                            }
                            let s = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("json_parse expects String".into()),
                            };
                            json_to_val(
                                &serde_json::from_str::<JsonValue>(s).map_err(|e| e.to_string())?,
                            )
                        }
                        "regex_findall" => {
                            if a.len() != 2 {
                                return Err("regex_findall expects pattern,text".into());
                            }
                            let p = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("pattern must be String".into()),
                            };
                            let t = match &a[1] {
                                Val::Str(s) => s,
                                _ => return Err("text must be String".into()),
                            };
                            let re = Regex::new(p).map_err(|e| e.to_string())?;
                            Val::List(
                                re.find_iter(t)
                                    .map(|m| Val::Str(m.as_str().to_string()))
                                    .collect(),
                            )
                        }
                        "vec_sum" => {
                            if a.len() != 1 {
                                return Err("vec_sum expects list".into());
                            }
                            let xs = as_num_list(&a[0])?;
                            Val::Num(xs.iter().sum())
                        }
                        "vec_mean" => {
                            if a.len() != 1 {
                                return Err("vec_mean expects list".into());
                            }
                            let xs = as_num_list(&a[0])?;
                            if xs.is_empty() {
                                return Err("vec_mean requires non-empty list".into());
                            }
                            Val::Num(xs.iter().sum::<f64>() / xs.len() as f64)
                        }
                        "vec_variance" => {
                            if a.len() != 1 {
                                return Err("vec_variance expects list".into());
                            }
                            let xs = as_num_list(&a[0])?;
                            if xs.is_empty() {
                                return Err("vec_variance requires non-empty list".into());
                            }
                            let m = xs.iter().sum::<f64>() / xs.len() as f64;
                            Val::Num(
                                xs.iter().map(|x| (x - m) * (x - m)).sum::<f64>() / xs.len() as f64,
                            )
                        }
                        "vec_std" => {
                            if a.len() != 1 {
                                return Err("vec_std expects list".into());
                            }
                            let xs = as_num_list(&a[0])?;
                            if xs.is_empty() {
                                return Err("vec_std requires non-empty list".into());
                            }
                            let m = xs.iter().sum::<f64>() / xs.len() as f64;
                            let v =
                                xs.iter().map(|x| (x - m) * (x - m)).sum::<f64>() / xs.len() as f64;
                            Val::Num(v.sqrt())
                        }
                        "vec_dot" => {
                            if a.len() != 2 {
                                return Err("vec_dot expects two lists".into());
                            }
                            let x = as_num_list(&a[0])?;
                            let y = as_num_list(&a[1])?;
                            if x.len() != y.len() {
                                return Err("vec_dot length mismatch".into());
                            }
                            Val::Num(x.iter().zip(y.iter()).map(|(a, b)| a * b).sum())
                        }
                        "vec_add" => {
                            if a.len() != 2 {
                                return Err("vec_add expects two lists".into());
                            }
                            let x = as_num_list(&a[0])?;
                            let y = as_num_list(&a[1])?;
                            if x.len() != y.len() {
                                return Err("vec_add length mismatch".into());
                            }
                            nums_to_val(x.iter().zip(y.iter()).map(|(a, b)| a + b).collect())
                        }
                        "vec_scale" => {
                            if a.len() != 2 {
                                return Err("vec_scale expects list,scalar".into());
                            }
                            let x = as_num_list(&a[0])?;
                            let s = match a[1] {
                                Val::Num(n) => n,
                                _ => return Err("scalar must be Number".into()),
                            };
                            nums_to_val(x.into_iter().map(|v| v * s).collect())
                        }
                        "transpose" => {
                            if a.len() != 1 {
                                return Err("transpose expects matrix".into());
                            }
                            let m = as_matrix(&a[0])?;
                            if m.is_empty() {
                                Val::List(vec![])
                            } else {
                                let cols = m[0].len();
                                if m.iter().any(|r| r.len() != cols) {
                                    return Err("ragged matrix".into());
                                }
                                let mut out = vec![vec![0.0; m.len()]; cols];
                                for i in 0..m.len() {
                                    for j in 0..cols {
                                        out[j][i] = m[i][j];
                                    }
                                }
                                matrix_to_val(out)
                            }
                        }
                        "matmul" => {
                            if a.len() != 2 {
                                return Err("matmul expects two matrices".into());
                            }
                            let x = as_matrix(&a[0])?;
                            let y = as_matrix(&a[1])?;
                            if x.is_empty() || y.is_empty() {
                                return Err("matmul requires non-empty matrices".into());
                            }
                            let n = x[0].len();
                            if x.iter().any(|r| r.len() != n)
                                || y.iter().any(|r| r.len() != y[0].len())
                                || y.len() != n
                            {
                                return Err("matmul dimension mismatch".into());
                            }
                            let p = y[0].len();
                            let mut out = vec![vec![0.0; p]; x.len()];
                            for i in 0..x.len() {
                                for j in 0..p {
                                    for k in 0..n {
                                        out[i][j] += x[i][k] * y[k][j];
                                    }
                                }
                            }
                            matrix_to_val(out)
                        }
                        "linreg_fit" => {
                            if a.len() != 2 {
                                return Err("linreg_fit expects x,y".into());
                            }
                            let x = as_num_list(&a[0])?;
                            let y = as_num_list(&a[1])?;
                            if x.len() != y.len() || x.len() < 2 {
                                return Err("linreg_fit requires equal lists length >=2".into());
                            }
                            let mx = x.iter().sum::<f64>() / x.len() as f64;
                            let my = y.iter().sum::<f64>() / y.len() as f64;
                            let num = x
                                .iter()
                                .zip(y.iter())
                                .map(|(a, b)| (a - mx) * (b - my))
                                .sum::<f64>();
                            let den = x.iter().map(|a| (a - mx) * (a - mx)).sum::<f64>();
                            if den == 0.0 {
                                return Err("linreg_fit zero x variance".into());
                            }
                            let slope = num / den;
                            let intercept = my - slope * mx;
                            let ss_tot = y.iter().map(|v| (v - my) * (v - my)).sum::<f64>();
                            let ss_res = x
                                .iter()
                                .zip(y.iter())
                                .map(|(xx, yy)| {
                                    let e = yy - (intercept + slope * xx);
                                    e * e
                                })
                                .sum::<f64>();
                            let r2 = if ss_tot == 0.0 {
                                1.0
                            } else {
                                1.0 - ss_res / ss_tot
                            };
                            Val::Map(vec![
                                (Val::Str("slope".into()), Val::Num(slope)),
                                (Val::Str("intercept".into()), Val::Num(intercept)),
                                (Val::Str("r2".into()), Val::Num(r2)),
                            ])
                        }
                        "linreg_predict" => {
                            if a.len() != 2 {
                                return Err("linreg_predict expects model,x".into());
                            }
                            let pairs = match &a[0] {
                                Val::Map(p) => p,
                                _ => return Err("model must be Map".into()),
                            };
                            let getn = |name: &str| -> Result<f64, String> {
                                pairs
                                    .iter()
                                    .find(|(k, _)| *k == Val::Str(name.into()))
                                    .and_then(
                                        |(_, v)| if let Val::Num(n) = v { Some(*n) } else { None },
                                    )
                                    .ok_or(format!("model missing {name}"))
                            };
                            let x = match a[1] {
                                Val::Num(n) => n,
                                _ => return Err("x must be Number".into()),
                            };
                            Val::Num(getn("intercept")? + getn("slope")? * x)
                        }
                        "knn_predict" => {
                            if a.len() != 4 {
                                return Err("knn_predict expects train_x,train_y,point,k".into());
                            }
                            let xs = as_matrix(&a[0])?;
                            let ys = match &a[1] {
                                Val::List(v) => v.clone(),
                                _ => return Err("train_y must be List".into()),
                            };
                            let point = as_num_list(&a[2])?;
                            let k = match a[3] {
                                Val::Num(n) => n as usize,
                                _ => return Err("k must be Number".into()),
                            };
                            if xs.len() != ys.len() || xs.is_empty() || k == 0 {
                                return Err("invalid knn data".into());
                            }
                            let mut pairs: Vec<(f64, Val)> = xs
                                .iter()
                                .zip(ys.iter())
                                .map(|(x, y)| (distance_sq(x, &point), y.clone()))
                                .collect();
                            pairs.sort_by(|a, b| {
                                a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal)
                            });
                            let mut counts: Vec<(Val, usize)> = vec![];
                            for (_, label) in pairs.into_iter().take(k.min(xs.len())) {
                                if let Some((_, c)) = counts.iter_mut().find(|(l, _)| *l == label) {
                                    *c += 1
                                } else {
                                    counts.push((label, 1))
                                }
                            }
                            counts
                                .into_iter()
                                .max_by_key(|(_, c)| *c)
                                .map(|(l, _)| l)
                                .unwrap_or(Val::Null)
                        }
                        "sigmoid" => {
                            if a.len() != 1 {
                                return Err("sigmoid expects number".into());
                            }
                            let x = match a[0] {
                                Val::Num(n) => n,
                                _ => return Err("sigmoid expects Number".into()),
                            };
                            Val::Num(1.0 / (1.0 + (-x).exp()))
                        }
                        "mse" => {
                            if a.len() != 2 {
                                return Err("mse expects actual,predicted".into());
                            }
                            let x = as_num_list(&a[0])?;
                            let y = as_num_list(&a[1])?;
                            if x.len() != y.len() || x.is_empty() {
                                return Err("mse length mismatch".into());
                            }
                            Val::Num(
                                x.iter()
                                    .zip(y.iter())
                                    .map(|(a, b)| (a - b) * (a - b))
                                    .sum::<f64>()
                                    / x.len() as f64,
                            )
                        }
                        "normalize" => {
                            if a.len() != 1 {
                                return Err("normalize expects list".into());
                            }
                            let xs = as_num_list(&a[0])?;
                            if xs.is_empty() {
                                return Err("normalize requires non-empty list".into());
                            }
                            let min = xs.iter().fold(f64::INFINITY, |m, x| m.min(*x));
                            let max = xs.iter().fold(f64::NEG_INFINITY, |m, x| m.max(*x));
                            let span = max - min;
                            if span == 0.0 {
                                nums_to_val(vec![0.0; xs.len()])
                            } else {
                                nums_to_val(xs.into_iter().map(|x| (x - min) / span).collect())
                            }
                        }
                        "relu" => {
                            if a.len() != 1 {
                                return Err("relu expects list".into());
                            }
                            nums_to_val(
                                as_num_list(&a[0])?
                                    .into_iter()
                                    .map(|x| x.max(0.0))
                                    .collect(),
                            )
                        }
                        "softmax" => {
                            if a.len() != 1 {
                                return Err("softmax expects list".into());
                            }
                            let xs = as_num_list(&a[0])?;
                            if xs.is_empty() {
                                return Err("softmax requires non-empty list".into());
                            }
                            let m = xs.iter().fold(f64::NEG_INFINITY, |m, x| m.max(*x));
                            let exps: Vec<f64> = xs.iter().map(|x| (x - m).exp()).collect();
                            let sum: f64 = exps.iter().sum();
                            nums_to_val(exps.into_iter().map(|x| x / sum).collect())
                        }
                        "dense" => {
                            if a.len() != 3 {
                                return Err("dense expects input,weights,bias".into());
                            }
                            let input = as_num_list(&a[0])?;
                            let weights = as_matrix(&a[1])?;
                            let bias = as_num_list(&a[2])?;
                            if weights.len() != bias.len()
                                || weights.iter().any(|r| r.len() != input.len())
                            {
                                return Err("dense dimension mismatch".into());
                            }
                            nums_to_val(
                                weights
                                    .iter()
                                    .zip(bias.iter())
                                    .map(|(row, b)| {
                                        row.iter()
                                            .zip(input.iter())
                                            .map(|(w, x)| w * x)
                                            .sum::<f64>()
                                            + b
                                    })
                                    .collect(),
                            )
                        }
                        "kmeans" => {
                            if a.len() != 3 {
                                return Err("kmeans expects data,k,iterations".into());
                            }
                            let data = as_matrix(&a[0])?;
                            let k = match a[1] {
                                Val::Num(n) => n as usize,
                                _ => return Err("k must be Number".into()),
                            };
                            let iters = match a[2] {
                                Val::Num(n) => n as usize,
                                _ => return Err("iterations must be Number".into()),
                            };
                            if data.is_empty() || k == 0 || k > data.len() {
                                return Err("invalid kmeans data".into());
                            }
                            let dims = data[0].len();
                            if dims == 0 || data.iter().any(|r| r.len() != dims) {
                                return Err("ragged kmeans data".into());
                            }
                            let mut centers = data[..k].to_vec();
                            for _ in 0..iters {
                                let mut sums = vec![vec![0.0; dims]; k];
                                let mut counts = vec![0usize; k];
                                for point in &data {
                                    let mut best = 0usize;
                                    let mut bestd = distance_sq(point, &centers[0]);
                                    for i in 1..k {
                                        let dist = distance_sq(point, &centers[i]);
                                        if dist < bestd {
                                            best = i;
                                            bestd = dist;
                                        }
                                    }
                                    counts[best] += 1;
                                    for j in 0..dims {
                                        sums[best][j] += point[j];
                                    }
                                }
                                for i in 0..k {
                                    if counts[i] > 0 {
                                        for j in 0..dims {
                                            centers[i][j] = sums[i][j] / counts[i] as f64;
                                        }
                                    }
                                }
                            }
                            matrix_to_val(centers)
                        }
                        "table_column" => {
                            if a.len() != 2 {
                                return Err("table_column expects rows,name".into());
                            }
                            let rows = match &a[0] {
                                Val::List(v) => v,
                                _ => return Err("rows must be List".into()),
                            };
                            let name = match &a[1] {
                                Val::Str(s) => Val::Str(s.clone()),
                                _ => return Err("name must be String".into()),
                            };
                            let mut out = vec![];
                            for row in rows {
                                let pairs = match row {
                                    Val::Map(p) => p,
                                    _ => return Err("rows must contain Map values".into()),
                                };
                                out.push(map_lookup(pairs, &name).cloned().unwrap_or(Val::Null));
                            }
                            Val::List(out)
                        }
                        "table_filter_eq" => {
                            if a.len() != 3 {
                                return Err("table_filter_eq expects rows,name,value".into());
                            }
                            let rows = match &a[0] {
                                Val::List(v) => v,
                                _ => return Err("rows must be List".into()),
                            };
                            let key = match &a[1] {
                                Val::Str(s) => Val::Str(s.clone()),
                                _ => return Err("name must be String".into()),
                            };
                            let target = &a[2];
                            Val::List(
                                rows.iter()
                                    .filter(|row| match row {
                                        Val::Map(p) => map_lookup(p, &key)
                                            .map(|v| v == target)
                                            .unwrap_or(false),
                                        _ => false,
                                    })
                                    .cloned()
                                    .collect(),
                            )
                        }
                        "table_describe" => {
                            if a.len() != 2 {
                                return Err("table_describe expects rows,name".into());
                            }
                            let rows = match &a[0] {
                                Val::List(v) => v,
                                _ => return Err("rows must be List".into()),
                            };
                            let key = match &a[1] {
                                Val::Str(s) => Val::Str(s.clone()),
                                _ => return Err("name must be String".into()),
                            };
                            let nums: Vec<f64> = rows
                                .iter()
                                .filter_map(|row| match row {
                                    Val::Map(p) => map_lookup(p, &key).and_then(|v| match v {
                                        Val::Num(n) => Some(n.to_owned()),
                                        _ => None,
                                    }),
                                    _ => None,
                                })
                                .collect();
                            if nums.is_empty() {
                                return Err("table_describe found no numeric values".into());
                            }
                            let count = nums.len() as f64;
                            let mean = nums.iter().sum::<f64>() / count;
                            let min = nums.iter().fold(f64::INFINITY, |m, x| m.min(*x));
                            let max = nums.iter().fold(f64::NEG_INFINITY, |m, x| m.max(*x));
                            Val::Map(vec![
                                (Val::Str("count".into()), Val::Num(count)),
                                (Val::Str("mean".into()), Val::Num(mean)),
                                (Val::Str("min".into()), Val::Num(min)),
                                (Val::Str("max".into()), Val::Num(max)),
                            ])
                        }
                        "plot_svg" => {
                            if a.len() != 3 {
                                return Err("plot_svg expects path,xs,ys".into());
                            }
                            let path = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("path must be String".into()),
                            };
                            let xs = as_num_list(&a[1])?;
                            let ys = as_num_list(&a[2])?;
                            if xs.len() != ys.len() || xs.len() < 2 {
                                return Err("plot_svg requires equal lists length >=2".into());
                            }
                            let xmin = xs.iter().fold(f64::INFINITY, |m, x| m.min(*x));
                            let xmax = xs.iter().fold(f64::NEG_INFINITY, |m, x| m.max(*x));
                            let ymin = ys.iter().fold(f64::INFINITY, |m, x| m.min(*x));
                            let ymax = ys.iter().fold(f64::NEG_INFINITY, |m, x| m.max(*x));
                            let sx = if xmax == xmin { 1.0 } else { xmax - xmin };
                            let sy = if ymax == ymin { 1.0 } else { ymax - ymin };
                            let pts = xs
                                .iter()
                                .zip(ys.iter())
                                .map(|(x, y)| {
                                    let px = 50.0 + 700.0 * (x - xmin) / sx;
                                    let py = 450.0 - 400.0 * (y - ymin) / sy;
                                    format!("{px:.2},{py:.2}")
                                })
                                .collect::<Vec<_>>()
                                .join(" ");
                            let svg = format!(
                                "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"800\" height=\"500\"><rect width=\"100%\" height=\"100%\" fill=\"white\"/><line x1=\"50\" y1=\"450\" x2=\"750\" y2=\"450\" stroke=\"black\"/><line x1=\"50\" y1=\"50\" x2=\"50\" y2=\"450\" stroke=\"black\"/><polyline fill=\"none\" stroke=\"black\" stroke-width=\"2\" points=\"{}\"/></svg>",
                                svg_escape(&pts)
                            );
                            fs::write(path, svg).map_err(|e| e.to_string())?;
                            Val::Null
                        }
                        "csv_read" => {
                            if a.len() != 1 {
                                return Err("csv_read expects path".into());
                            }
                            let p = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("path must be String".into()),
                            };
                            let mut rdr = csv::Reader::from_path(p).map_err(|e| e.to_string())?;
                            let headers = rdr
                                .headers()
                                .map_err(|e| e.to_string())?
                                .iter()
                                .map(|s| s.to_string())
                                .collect::<Vec<_>>();
                            let mut rows = vec![];
                            for rec in rdr.records() {
                                let rec = rec.map_err(|e| e.to_string())?;
                                let pairs = headers
                                    .iter()
                                    .zip(rec.iter())
                                    .map(|(h, v)| {
                                        let val = v
                                            .parse::<f64>()
                                            .map(Val::Num)
                                            .unwrap_or_else(|_| Val::Str(v.to_string()));
                                        (Val::Str(h.clone()), val)
                                    })
                                    .collect();
                                rows.push(Val::Map(pairs));
                            }
                            Val::List(rows)
                        }
                        "csv_write" => {
                            if a.len() != 2 {
                                return Err("csv_write expects path,rows".into());
                            }
                            let p = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("path must be String".into()),
                            };
                            let rows = match &a[1] {
                                Val::List(v) => v,
                                _ => return Err("rows must be List".into()),
                            };
                            let mut w = csv::Writer::from_path(p).map_err(|e| e.to_string())?;
                            if let Some(Val::Map(first)) = rows.first() {
                                let headers =
                                    first.iter().map(|(k, _)| k.show()).collect::<Vec<_>>();
                                w.write_record(&headers).map_err(|e| e.to_string())?;
                                for row in rows {
                                    let map = match row {
                                        Val::Map(p) => p,
                                        _ => return Err("all csv rows must be Map".into()),
                                    };
                                    let vals = headers
                                        .iter()
                                        .map(|h| {
                                            map.iter()
                                                .find(|(k, _)| k.show() == *h)
                                                .map(|(_, v)| v.show())
                                                .unwrap_or_default()
                                        })
                                        .collect::<Vec<_>>();
                                    w.write_record(vals).map_err(|e| e.to_string())?;
                                }
                            }
                            w.flush().map_err(|e| e.to_string())?;
                            Val::Null
                        }
                        "gui_message" => {
                            if a.len() != 2 {
                                return Err("gui_message expects title,message".into());
                            }
                            let title = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("title must be String".into()),
                            };
                            let msg = match &a[1] {
                                Val::Str(s) => s,
                                _ => return Err("message must be String".into()),
                            };
                            tinyfiledialogs::message_box_ok(
                                title,
                                msg,
                                tinyfiledialogs::MessageBoxIcon::Info,
                            );
                            Val::Null
                        }
                        "gui_input" => {
                            if a.len() != 2 {
                                return Err("gui_input expects title,message".into());
                            }
                            let title = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("title must be String".into()),
                            };
                            let msg = match &a[1] {
                                Val::Str(s) => s,
                                _ => return Err("message must be String".into()),
                            };
                            tinyfiledialogs::input_box(title, msg, "")
                                .map(Val::Str)
                                .unwrap_or(Val::Null)
                        }
                        "gui_open_file" => {
                            if a.len() != 1 {
                                return Err("gui_open_file expects title".into());
                            }
                            let title = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("title must be String".into()),
                            };
                            tinyfiledialogs::open_file_dialog(title, "", None)
                                .map(Val::Str)
                                .unwrap_or(Val::Null)
                        }
                        "gui_save_file" => {
                            if a.len() != 1 {
                                return Err("gui_save_file expects title".into());
                            }
                            let title = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("title must be String".into()),
                            };
                            tinyfiledialogs::save_file_dialog(title, "")
                                .map(Val::Str)
                                .unwrap_or(Val::Null)
                        }
                        "gui_confirm" => {
                            if a.len() != 2 {
                                return Err("gui_confirm expects title,message".into());
                            }
                            let title = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("title must be String".into()),
                            };
                            let msg = match &a[1] {
                                Val::Str(s) => s,
                                _ => return Err("message must be String".into()),
                            };
                            Val::Bool(matches!(
                                tinyfiledialogs::message_box_yes_no(
                                    title,
                                    msg,
                                    tinyfiledialogs::MessageBoxIcon::Question,
                                    tinyfiledialogs::YesNo::Yes
                                ),
                                tinyfiledialogs::YesNo::Yes
                            ))
                        }
                        "gui_password" => {
                            if a.len() != 2 {
                                return Err("gui_password expects title,message".into());
                            }
                            let title = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("title must be String".into()),
                            };
                            let msg = match &a[1] {
                                Val::Str(s) => s,
                                _ => return Err("message must be String".into()),
                            };
                            tinyfiledialogs::password_box(title, msg)
                                .map(Val::Str)
                                .unwrap_or(Val::Null)
                        }
                        "gui_folder" => {
                            if a.len() != 1 {
                                return Err("gui_folder expects title".into());
                            }
                            let title = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("title must be String".into()),
                            };
                            tinyfiledialogs::select_folder_dialog(title, "")
                                .map(Val::Str)
                                .unwrap_or(Val::Null)
                        }
                        "gui_color" => {
                            if a.len() != 1 {
                                return Err("gui_color expects title".into());
                            }
                            let title = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("title must be String".into()),
                            };
                            match tinyfiledialogs::color_chooser_dialog(
                                title,
                                tinyfiledialogs::DefaultColorValue::Hex("#ffffff"),
                            ) {
                                Some((hex, _)) => Val::Str(hex),
                                None => Val::Null,
                            }
                        }
                        "gui_form" => {
                            if a.len() != 2 {
                                return Err("gui_form expects title,fields".into());
                            }
                            let title = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("title must be String".into()),
                            };
                            let fields = match &a[1] {
                                Val::List(v) => v,
                                _ => return Err("fields must be List".into()),
                            };
                            let mut out = vec![];
                            for field in fields {
                                let label = match field {
                                    Val::Str(s) => s,
                                    _ => return Err("field names must be String".into()),
                                };
                                match tinyfiledialogs::input_box(title, label, "") {
                                    Some(value) => {
                                        out.push((Val::Str(label.clone()), Val::Str(value)))
                                    }
                                    None => return Ok(Val::Null),
                                }
                            }
                            Val::Map(out)
                        }
                        "sqrt" => {
                            if a.len() != 1 {
                                return Err("sqrt expects 1 argument".into());
                            }
                            match a[0] {
                                Val::Num(n) => Val::Num(n.sqrt()),
                                _ => return Err("sqrt expects Number".into()),
                            }
                        }
                        "abs" => {
                            if a.len() != 1 {
                                return Err("abs expects 1 argument".into());
                            }
                            match a[0] {
                                Val::Num(n) => Val::Num(n.abs()),
                                _ => return Err("abs expects Number".into()),
                            }
                        }
                        "floor" => {
                            if a.len() != 1 {
                                return Err("floor expects 1 argument".into());
                            }
                            match a[0] {
                                Val::Num(n) => Val::Num(n.floor()),
                                _ => return Err("floor expects Number".into()),
                            }
                        }
                        "ceil" => {
                            if a.len() != 1 {
                                return Err("ceil expects 1 argument".into());
                            }
                            match a[0] {
                                Val::Num(n) => Val::Num(n.ceil()),
                                _ => return Err("ceil expects Number".into()),
                            }
                        }
                        "lower" => {
                            if a.len() != 1 {
                                return Err("lower expects String".into());
                            }
                            match &a[0] {
                                Val::Str(s) => Val::Str(s.to_lowercase()),
                                _ => return Err("lower expects String".into()),
                            }
                        }
                        "upper" => {
                            if a.len() != 1 {
                                return Err("upper expects String".into());
                            }
                            match &a[0] {
                                Val::Str(s) => Val::Str(s.to_uppercase()),
                                _ => return Err("upper expects String".into()),
                            }
                        }
                        "trim" => {
                            if a.len() != 1 {
                                return Err("trim expects String".into());
                            }
                            match &a[0] {
                                Val::Str(s) => Val::Str(s.trim().to_string()),
                                _ => return Err("trim expects String".into()),
                            }
                        }
                        "split" => {
                            if a.len() != 2 {
                                return Err("split expects text,separator".into());
                            }
                            let text = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("text must be String".into()),
                            };
                            let sep = match &a[1] {
                                Val::Str(s) => s,
                                _ => return Err("separator must be String".into()),
                            };
                            Val::List(text.split(sep).map(|x| Val::Str(x.to_string())).collect())
                        }
                        "join" => {
                            if a.len() != 2 {
                                return Err("join expects list,separator".into());
                            }
                            let xs = match &a[0] {
                                Val::List(xs) => xs,
                                _ => return Err("first argument must be List".into()),
                            };
                            let sep = match &a[1] {
                                Val::Str(s) => s,
                                _ => return Err("separator must be String".into()),
                            };
                            Val::Str(xs.iter().map(Val::show).collect::<Vec<_>>().join(sep))
                        }
                        "contains" => {
                            if a.len() != 2 {
                                return Err("contains expects container,value".into());
                            }
                            Val::Bool(match &a[0] {
                                Val::Str(s) => match &a[1] {
                                    Val::Str(x) => s.contains(x),
                                    _ => false,
                                },
                                Val::List(xs) => xs.contains(&a[1]),
                                Val::Map(pairs) => pairs.iter().any(|(k, _)| *k == a[1]),
                                _ => false,
                            })
                        }
                        "read_file" => {
                            if a.len() != 1 {
                                return Err("read_file expects path".into());
                            }
                            let p = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("path must be String".into()),
                            };
                            Val::Str(fs::read_to_string(p).map_err(|e| e.to_string())?)
                        }
                        "write_file" => {
                            if a.len() != 2 {
                                return Err("write_file expects path,data".into());
                            }
                            let p = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("path must be String".into()),
                            };
                            let data = a[1].show();
                            fs::write(p, data).map_err(|e| e.to_string())?;
                            Val::Null
                        }
                        "path_exists" => {
                            if a.len() != 1 {
                                return Err("path_exists expects path".into());
                            }
                            let p = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("path must be String".into()),
                            };
                            Val::Bool(Path::new(p).exists())
                        }
                        "cwd" => Val::Str(
                            env::current_dir()
                                .map_err(|e| e.to_string())?
                                .display()
                                .to_string(),
                        ),
                        "env_get" => {
                            if a.len() != 1 {
                                return Err("env_get expects key".into());
                            }
                            let k = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("key must be String".into()),
                            };
                            env::var(k).map(Val::Str).unwrap_or(Val::Null)
                        }
                        "clock" => {
                            let t = SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .map_err(|e| e.to_string())?;
                            Val::Num(t.as_secs_f64())
                        }
                        "random" => Val::Num(rand::thread_rng().r#gen::<f64>()),
                        "randint" => {
                            if a.len() != 2 {
                                return Err("randint expects min,max".into());
                            }
                            let lo = match a[0] {
                                Val::Num(n) => n as i64,
                                _ => return Err("min must be Number".into()),
                            };
                            let hi = match a[1] {
                                Val::Num(n) => n as i64,
                                _ => return Err("max must be Number".into()),
                            };
                            Val::Num(rand::thread_rng().gen_range(lo..=hi) as f64)
                        }
                        "sha256" => {
                            if a.len() != 1 {
                                return Err("sha256 expects value".into());
                            }
                            let mut h = Sha256::new();
                            h.update(a[0].show().as_bytes());
                            Val::Str(format!("{:x}", h.finalize()))
                        }
                        "shell" => {
                            if a.len() != 1 {
                                return Err("shell expects command".into());
                            }
                            let cmd = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("command must be String".into()),
                            };
                            let out = if cfg!(target_os = "windows") {
                                Command::new("cmd").args(["/C", cmd]).output()
                            } else {
                                Command::new("sh").args(["-c", cmd]).output()
                            }
                            .map_err(|e| e.to_string())?;
                            Val::Map(vec![
                                (
                                    Val::Str("code".into()),
                                    Val::Num(out.status.code().unwrap_or(-1) as f64),
                                ),
                                (
                                    Val::Str("out".into()),
                                    Val::Str(String::from_utf8_lossy(&out.stdout).to_string()),
                                ),
                                (
                                    Val::Str("err".into()),
                                    Val::Str(String::from_utf8_lossy(&out.stderr).to_string()),
                                ),
                            ])
                        }
                        "http_get" => {
                            if a.len() != 1 {
                                return Err("http_get expects url".into());
                            }
                            let url = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("url must be String".into()),
                            };
                            let text = reqwest::blocking::get(url)
                                .map_err(|e| e.to_string())?
                                .text()
                                .map_err(|e| e.to_string())?;
                            Val::Str(text)
                        }
                        "sqlite_exec" => {
                            if a.len() != 2 {
                                return Err("sqlite_exec expects path,sql".into());
                            }
                            let path = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("path must be String".into()),
                            };
                            let sql = match &a[1] {
                                Val::Str(s) => s,
                                _ => return Err("sql must be String".into()),
                            };
                            let conn = Connection::open(path).map_err(|e| e.to_string())?;
                            let n = conn.execute(sql, []).map_err(|e| e.to_string())?;
                            Val::Num(n as f64)
                        }
                        "sqlite_query" => {
                            if a.len() != 2 {
                                return Err("sqlite_query expects path,sql".into());
                            }
                            let path = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("path must be String".into()),
                            };
                            let sql = match &a[1] {
                                Val::Str(s) => s,
                                _ => return Err("sql must be String".into()),
                            };
                            let conn = Connection::open(path).map_err(|e| e.to_string())?;
                            let mut stmt = conn.prepare(sql).map_err(|e| e.to_string())?;
                            let names: Vec<String> =
                                stmt.column_names().iter().map(|s| s.to_string()).collect();
                            let rows = stmt
                                .query_map([], |row| {
                                    let mut pairs = Vec::new();
                                    for (i, name) in names.iter().enumerate() {
                                        let val = match row.get_ref(i)? {
                                            rusqlite::types::ValueRef::Null => Val::Null,
                                            rusqlite::types::ValueRef::Integer(x) => {
                                                Val::Num(x as f64)
                                            }
                                            rusqlite::types::ValueRef::Real(x) => Val::Num(x),
                                            rusqlite::types::ValueRef::Text(x) => {
                                                Val::Str(String::from_utf8_lossy(x).to_string())
                                            }
                                            rusqlite::types::ValueRef::Blob(x) => {
                                                Val::Str(format!("<{} bytes>", x.len()))
                                            }
                                        };
                                        pairs.push((Val::Str(name.clone()), val));
                                    }
                                    Ok(Val::Map(pairs))
                                })
                                .map_err(|e| e.to_string())?;
                            let mut out = Vec::new();
                            for row in rows {
                                out.push(row.map_err(|e| e.to_string())?);
                            }
                            Val::List(out)
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
                        let target =
                            if !rel.contains('/') && !rel.contains('\\') && !rel.ends_with(".vy") {
                                if let Ok(root) = project_root(base) {
                                    let manifest_path = root.join("veyra.json");
                                    let manifest: JsonValue = serde_json::from_str(
                                        &fs::read_to_string(&manifest_path)
                                            .map_err(|e| e.to_string())?,
                                    )
                                    .map_err(|e| e.to_string())?;
                                    if let Some(meta) =
                                        manifest.get("packages").and_then(|p| p.get(rel))
                                    {
                                        let entry = meta
                                            .get("entry")
                                            .and_then(JsonValue::as_str)
                                            .unwrap_or("main.vy");
                                        root.join("vendor").join(rel).join(entry)
                                    } else {
                                        base.join(rel)
                                    }
                                } else {
                                    base.join(rel)
                                }
                            } else {
                                base.join(rel)
                            };
                        out.push_str(&load(&target, seen)?);
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

fn copy_dir_all(src: &Path, dst: &Path) -> Result<(), String> {
    fs::create_dir_all(dst).map_err(|e| e.to_string())?;
    for entry in fs::read_dir(src).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let ty = entry.file_type().map_err(|e| e.to_string())?;
        let target = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn project_root(start: &Path) -> Result<PathBuf, String> {
    let mut p = start.canonicalize().unwrap_or_else(|_| start.to_path_buf());
    loop {
        if p.join("veyra.json").exists() {
            return Ok(p);
        }
        if !p.pop() {
            return Err("veyra.json not found".into());
        }
    }
}

fn veyra_home() -> PathBuf {
    if let Ok(v) = env::var("VEYRA_HOME") {
        return PathBuf::from(v);
    }
    if cfg!(target_os = "windows") {
        if let Ok(v) = env::var("LOCALAPPDATA") {
            return PathBuf::from(v).join("Veyra");
        }
    }
    env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn pkg_index() -> Result<(PathBuf, JsonValue), String> {
    let path = veyra_home().join("registry").join("index.json");
    let text = fs::read_to_string(&path)
        .map_err(|e| format!("registry unavailable at {}: {e}", path.display()))?;
    let json = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    Ok((path, json))
}

fn write_package_lock(root: &Path, manifest: &JsonValue) -> Result<(), String> {
    let lock = serde_json::json!({
        "format": 1,
        "engine": VERSION,
        "packages": manifest.get("packages").cloned().unwrap_or_else(|| serde_json::json!({}))
    });
    fs::write(
        root.join("veyra.lock"),
        serde_json::to_string_pretty(&lock).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())
}

fn pkg_add(name: &str) -> Result<(), String> {
    let root = project_root(&env::current_dir().map_err(|e| e.to_string())?)?;
    let (index_path, index) = pkg_index()?;
    let pkg = index
        .get("packages")
        .and_then(|x| x.get(name))
        .ok_or(format!("package '{name}' not found in Veyra registry"))?;
    let version = pkg
        .get("version")
        .and_then(JsonValue::as_str)
        .unwrap_or("0.0.0");
    let entry = pkg
        .get("entry")
        .and_then(JsonValue::as_str)
        .unwrap_or("main.vy");
    let source = pkg
        .get("source")
        .and_then(JsonValue::as_str)
        .ok_or("registry package missing source")?;
    let source_path = index_path.parent().unwrap_or(Path::new(".")).join(source);
    let dest = root.join("vendor").join(name);
    if dest.exists() {
        fs::remove_dir_all(&dest).map_err(|e| e.to_string())?;
    }
    copy_dir_all(&source_path, &dest)?;

    let manifest_path = root.join("veyra.json");
    let mut manifest: JsonValue =
        serde_json::from_str(&fs::read_to_string(&manifest_path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    if !manifest
        .get("packages")
        .map(JsonValue::is_object)
        .unwrap_or(false)
    {
        manifest["packages"] = JsonValue::Object(serde_json::Map::new());
    }
    manifest["packages"][name] = serde_json::json!({"version":version,"entry":entry});
    fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    write_package_lock(&root, &manifest)?;
    println!("Added {name} {version} -> vendor/{name}/{entry}");
    Ok(())
}

fn pkg_remove(name: &str) -> Result<(), String> {
    let root = project_root(&env::current_dir().map_err(|e| e.to_string())?)?;
    let dest = root.join("vendor").join(name);
    if dest.exists() {
        fs::remove_dir_all(&dest).map_err(|e| e.to_string())?;
    }
    let manifest_path = root.join("veyra.json");
    let mut manifest: JsonValue =
        serde_json::from_str(&fs::read_to_string(&manifest_path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    if let Some(obj) = manifest
        .get_mut("packages")
        .and_then(JsonValue::as_object_mut)
    {
        obj.remove(name);
    }
    fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    write_package_lock(&root, &manifest)?;
    println!("Removed {name}");
    Ok(())
}

fn pkg_list() -> Result<(), String> {
    let (_, index) = pkg_index()?;
    println!("Veyra Registry");
    if let Some(pkgs) = index.get("packages").and_then(JsonValue::as_object) {
        for (name, meta) in pkgs {
            let version = meta
                .get("version")
                .and_then(JsonValue::as_str)
                .unwrap_or("?");
            println!("  {name} {version}");
        }
    }
    Ok(())
}

fn pkg_install() -> Result<(), String> {
    let root = project_root(&env::current_dir().map_err(|e| e.to_string())?)?;
    let manifest: JsonValue = serde_json::from_str(
        &fs::read_to_string(root.join("veyra.json")).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let names: Vec<String> = manifest
        .get("packages")
        .and_then(JsonValue::as_object)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    for name in names {
        pkg_add(&name)?;
    }
    println!("Veyra packages restored");
    Ok(())
}

fn pkg_search(term: &str) -> Result<(), String> {
    let (_, index) = pkg_index()?;
    println!("Veyra Registry search: {term}");
    if let Some(pkgs) = index.get("packages").and_then(JsonValue::as_object) {
        for (name, meta) in pkgs {
            if name.to_lowercase().contains(&term.to_lowercase()) {
                let version = meta
                    .get("version")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("?");
                println!("  {name} {version}");
            }
        }
    }
    Ok(())
}

fn pkg_publish() -> Result<(), String> {
    let root = project_root(&env::current_dir().map_err(|e| e.to_string())?)?;
    let manifest_path = root.join("veyra.json");
    let manifest: JsonValue =
        serde_json::from_str(&fs::read_to_string(&manifest_path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let name = manifest
        .get("name")
        .and_then(JsonValue::as_str)
        .ok_or("manifest missing name")?;
    let version = manifest
        .get("version")
        .and_then(JsonValue::as_str)
        .unwrap_or("0.0.0");
    let entry = manifest
        .get("entry")
        .and_then(JsonValue::as_str)
        .unwrap_or("src/main.vy");

    let home = veyra_home();
    let pkg_root = home.join("packages").join(name);
    if pkg_root.exists() {
        fs::remove_dir_all(&pkg_root).map_err(|e| e.to_string())?;
    }
    fs::create_dir_all(&pkg_root).map_err(|e| e.to_string())?;
    copy_dir_all(&root.join("src"), &pkg_root.join("src"))?;

    let index_path = home.join("registry").join("index.json");
    fs::create_dir_all(index_path.parent().unwrap_or(Path::new("."))).map_err(|e| e.to_string())?;
    let mut index: JsonValue = if index_path.exists() {
        serde_json::from_str(&fs::read_to_string(&index_path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?
    } else {
        serde_json::json!({"registry_version":1,"packages":{}})
    };
    if !index
        .get("packages")
        .map(JsonValue::is_object)
        .unwrap_or(false)
    {
        index["packages"] = JsonValue::Object(serde_json::Map::new());
    }
    index["packages"][name] = serde_json::json!({
        "version": version,
        "entry": entry,
        "source": format!("../packages/{name}")
    });
    fs::write(
        &index_path,
        serde_json::to_string_pretty(&index).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    println!("Published {name} {version} to {}", index_path.display());
    Ok(())
}

fn project_packages() -> Result<(), String> {
    let root = project_root(&env::current_dir().map_err(|e| e.to_string())?)?;
    let manifest: JsonValue = serde_json::from_str(
        &fs::read_to_string(root.join("veyra.json")).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    println!("Installed Veyra packages");
    if let Some(pkgs) = manifest.get("packages").and_then(JsonValue::as_object) {
        for (name, meta) in pkgs {
            let version = meta
                .get("version")
                .and_then(JsonValue::as_str)
                .unwrap_or("?");
            println!("  {name} {version}");
        }
    }
    Ok(())
}

fn project_entry() -> Result<PathBuf, String> {
    let root = project_root(&env::current_dir().map_err(|e| e.to_string())?)?;
    let manifest: JsonValue = serde_json::from_str(
        &fs::read_to_string(root.join("veyra.json")).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let entry = manifest
        .get("entry")
        .and_then(JsonValue::as_str)
        .unwrap_or("src/main.vy");
    Ok(root.join(entry))
}

fn test_project() -> Result<(), String> {
    let root = project_root(&env::current_dir().map_err(|e| e.to_string())?)?;
    let dir = root.join("tests");
    if !dir.exists() {
        println!("No tests directory");
        return Ok(());
    }
    let mut total = 0usize;
    let mut passed = 0usize;
    for entry in fs::read_dir(&dir).map_err(|e| e.to_string())? {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.extension().and_then(|x| x.to_str()) != Some("vy") {
            continue;
        }
        total += 1;
        match load_source(path.to_string_lossy().as_ref()).and_then(|s| run_source(&s)) {
            Ok(()) => {
                passed += 1;
                println!("PASS {}", path.display());
            }
            Err(e) => println!("FAIL {}: {e}", path.display()),
        }
    }
    println!("{passed}/{total} passed");
    if passed != total {
        return Err("test suite failed".into());
    }
    Ok(())
}

fn build_project() -> Result<(), String> {
    let root = project_root(&env::current_dir().map_err(|e| e.to_string())?)?;
    let manifest_path = root.join("veyra.json");
    let manifest: JsonValue =
        serde_json::from_str(&fs::read_to_string(&manifest_path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let name = manifest
        .get("name")
        .and_then(JsonValue::as_str)
        .unwrap_or("app");
    let version = manifest
        .get("version")
        .and_then(JsonValue::as_str)
        .unwrap_or("0.0.0");
    let dist = root.join("dist").join(format!("{name}-{version}"));
    if dist.exists() {
        fs::remove_dir_all(&dist).map_err(|e| e.to_string())?;
    }
    fs::create_dir_all(&dist).map_err(|e| e.to_string())?;
    copy_dir_all(&root.join("src"), &dist.join("src"))?;
    if root.join("vendor").exists() {
        copy_dir_all(&root.join("vendor"), &dist.join("vendor"))?;
    }
    fs::copy(&manifest_path, dist.join("veyra.json")).map_err(|e| e.to_string())?;
    let exe = env::current_exe().map_err(|e| e.to_string())?;
    fs::copy(
        &exe,
        dist.join(if cfg!(target_os = "windows") {
            "veyra.exe"
        } else {
            "veyra"
        }),
    )
    .map_err(|e| e.to_string())?;
    println!("Built {}", dist.display());
    Ok(())
}

fn serve_directory(dir: &Path, port: u16) -> Result<(), String> {
    let server = Server::http(format!("0.0.0.0:{port}")).map_err(|e| e.to_string())?;
    println!("Veyra web server: http://127.0.0.1:{port}");
    println!("Serving {}", dir.display());
    for req in server.incoming_requests() {
        let raw = req.url().split('?').next().unwrap_or("/");
        let rel = raw.trim_start_matches('/');
        let mut path = dir.join(if rel.is_empty() { "index.html" } else { rel });
        if path.is_dir() {
            path = path.join("index.html");
        }
        let response = if path.exists() && path.is_file() {
            let data = fs::read(&path).map_err(|e| e.to_string())?;
            let mut r = Response::from_data(data);
            let content_type = match path.extension().and_then(|x| x.to_str()).unwrap_or("") {
                "html" => "text/html; charset=utf-8",
                "css" => "text/css; charset=utf-8",
                "js" => "application/javascript; charset=utf-8",
                "json" => "application/json; charset=utf-8",
                "png" => "image/png",
                "jpg" | "jpeg" => "image/jpeg",
                "svg" => "image/svg+xml",
                _ => "application/octet-stream",
            };
            if let Ok(h) = Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes()) {
                r.add_header(h);
            }
            r
        } else {
            Response::from_string("404 Not Found").with_status_code(StatusCode(404))
        };
        req.respond(response).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn serve_routes(routes_file: &Path, port: u16) -> Result<(), String> {
    let config: JsonValue =
        serde_json::from_str(&fs::read_to_string(routes_file).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let routes = config
        .get("routes")
        .and_then(JsonValue::as_object)
        .ok_or("web route file requires a 'routes' object")?;
    let server = Server::http(format!("0.0.0.0:{port}")).map_err(|e| e.to_string())?;
    println!("Veyra API server: http://127.0.0.1:{port}");
    println!("Routes: {}", routes.len());
    for mut req in server.incoming_requests() {
        let path = req.url().split('?').next().unwrap_or("/");
        let method = req.method().as_str();
        let method_key = format!("{method} {path}");
        let route = routes.get(&method_key).or_else(|| routes.get(path));
        let response = if let Some(route) = route {
            let status = route
                .get("status")
                .and_then(JsonValue::as_u64)
                .unwrap_or(200) as u16;
            let content_type = route
                .get("content_type")
                .and_then(JsonValue::as_str)
                .unwrap_or_else(|| {
                    if route.get("json").is_some() {
                        "application/json; charset=utf-8"
                    } else {
                        "text/plain; charset=utf-8"
                    }
                });
            let body = if route
                .get("echo")
                .and_then(JsonValue::as_bool)
                .unwrap_or(false)
            {
                let mut text = String::new();
                req.as_reader()
                    .read_to_string(&mut text)
                    .map_err(|e| e.to_string())?;
                text
            } else if let Some(j) = route.get("json") {
                serde_json::to_string(j).map_err(|e| e.to_string())?
            } else {
                route
                    .get("body")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("")
                    .to_string()
            };
            let mut r = Response::from_string(body).with_status_code(StatusCode(status));
            if let Ok(h) = Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes()) {
                r.add_header(h);
            }
            r
        } else {
            Response::from_string("404 Not Found").with_status_code(StatusCode(404))
        };
        req.respond(response).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn new_project(name: &str) -> Result<(), String> {
    let p = PathBuf::from(name);
    if p.exists() {
        return Err(format!("'{name}' already exists"));
    }
    let project_name = p
        .file_name()
        .and_then(|x| x.to_str())
        .filter(|x| !x.is_empty())
        .unwrap_or("veyra-app")
        .to_string();
    fs::create_dir_all(p.join("src")).map_err(|e| e.to_string())?;
    fs::create_dir_all(p.join("tests")).map_err(|e| e.to_string())?;
    fs::create_dir_all(p.join("vendor")).map_err(|e| e.to_string())?;
    let manifest = serde_json::json!({
        "name": project_name,
        "version": "0.1.0",
        "entry": "src/main.vy",
        "edition": "2026",
        "engine": VERSION,
        "packages": {}
    });
    fs::write(
        p.join("veyra.json"),
        serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?,
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
        "Veyra {VERSION}\n\nUsage:\n  veyra run [file.vy]\n  veyra check [file.vy]\n  veyra test\n  veyra build\n  veyra serve [dir] [port]\n  veyra web <routes.json> [port]\n  veyra disasm [file.vy]\n  veyra fmt <file.vy>\n  veyra new <name>\n  veyra pkg list\n  veyra pkg add <name>\n  veyra pkg remove <name>\n  veyra pkg installed\n  veyra doctor\n  veyra repl\n  veyra --version"
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
        Some("pkg") => match a.get(2).map(String::as_str) {
            Some("list") => pkg_list(),
            Some("installed") => project_packages(),
            Some("install") => pkg_install(),
            Some("publish") => pkg_publish(),
            Some("search") => a
                .get(3)
                .ok_or("missing search term".into())
                .and_then(|q| pkg_search(q)),
            Some("add") => a
                .get(3)
                .ok_or("missing package name".into())
                .and_then(|n| pkg_add(n)),
            Some("remove") => a
                .get(3)
                .ok_or("missing package name".into())
                .and_then(|n| pkg_remove(n)),
            _ => Err(
                "usage: veyra pkg <list|search|installed|install|publish|add|remove> [name]".into(),
            ),
        },
        Some("add") => a
            .get(2)
            .ok_or("missing package name".into())
            .and_then(|n| pkg_add(n)),
        Some("remove") => a
            .get(2)
            .ok_or("missing package name".into())
            .and_then(|n| pkg_remove(n)),
        Some("install") => pkg_install(),
        Some("publish") => pkg_publish(),
        Some("packages") => project_packages(),
        Some("registry") => pkg_list(),
        Some("check") => {
            let path = match a.get(2) {
                Some(p) => Ok(PathBuf::from(p)),
                None => project_entry(),
            };
            path.and_then(|p| load_source(p.to_string_lossy().as_ref()))
                .and_then(|s| compile_source(&s).map(|_| println!("OK")))
        }
        Some("run") => {
            let path = match a.get(2) {
                Some(p) => Ok(PathBuf::from(p)),
                None => project_entry(),
            };
            path.and_then(|p| load_source(p.to_string_lossy().as_ref()))
                .and_then(|s| run_source(&s))
        }
        Some("test") => test_project(),
        Some("build") => build_project(),
        Some("serve") => {
            let dir = a
                .get(2)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            let port = a.get(3).and_then(|x| x.parse::<u16>().ok()).unwrap_or(8080);
            serve_directory(&dir, port)
        }
        Some("web") => a.get(2).ok_or("missing routes.json".into()).and_then(|p| {
            let port = a.get(3).and_then(|x| x.parse::<u16>().ok()).unwrap_or(8080);
            serve_routes(Path::new(p), port)
        }),
        Some("disasm") => {
            let path = match a.get(2) {
                Some(p) => Ok(PathBuf::from(p)),
                None => project_entry(),
            };
            path.and_then(|p| load_source(p.to_string_lossy().as_ref()))
                .and_then(|s| disasm_source(&s))
        }
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
    #[test]
    fn native_json_regex_hash() {
        assert!(run_source(
            "let j=json_parse(\"{\\\"a\\\":7}\"); let x=get(j,\"a\"); let m=regex_findall(\"[0-9]+\",\"a12b34\"); let h=sha256(\"veyra\");"
        ).is_ok());
    }
    #[test]
    fn native_file_io() {
        let p = env::temp_dir().join("veyra_native_io_test.txt");
        let s = format!(
            "write_file(\"{}\",\"hello\"); let x=read_file(\"{}\");",
            p.to_string_lossy().replace('\\', "\\\\"),
            p.to_string_lossy().replace('\\', "\\\\")
        );
        assert!(run_source(&s).is_ok());
        let _ = fs::remove_file(p);
    }
    #[test]
    fn native_science_math() {
        assert!(run_source(
            "let x=[1,2,3,4]; let m=vec_mean(x); let s=vec_std(x); let d=vec_dot([1,2],[3,4]); let z=matmul([[1,2],[3,4]],[[5,6],[7,8]]);"
        ).is_ok());
    }
    #[test]
    fn native_ml() {
        assert!(run_source(
            "let model=linreg_fit([1,2,3,4],[3,5,7,9]); let p=linreg_predict(model,5); let c=knn_predict([[0,0],[1,1],[9,9]],[\"a\",\"a\",\"b\"],[0.5,0.5],2); let centers=kmeans([[0,0],[0,1],[9,9],[9,8]],2,5); let h=dense([1,2],[[1,0],[0,1]],[0,0]); let r=relu([-1,2]); let s=softmax([1,2,3]); let n=normalize([10,20,30]);"
        ).is_ok());
    }
    #[test]
    fn native_csv() {
        let p = env::temp_dir().join("veyra_native_data.csv");
        fs::write(&p, "x,y\n1,2\n3,4\n").unwrap();
        let s = format!(
            "let rows=csv_read(\"{}\"); let x=rows[0][\"x\"];",
            p.to_string_lossy().replace('\\', "\\\\")
        );
        assert!(run_source(&s).is_ok());
        let _ = fs::remove_file(p);
    }
    #[test]
    fn native_table_plot() {
        let p = env::temp_dir().join("veyra_native_plot.svg");
        let s = format!(
            "let rows=[{{\"x\":1,\"g\":\"a\"}},{{\"x\":3,\"g\":\"b\"}}]; let c=table_column(rows,\"x\"); let d=table_describe(rows,\"x\"); let f=table_filter_eq(rows,\"g\",\"a\"); plot_svg(\"{}\",[1,2,3],[2,4,3]);",
            p.to_string_lossy().replace('\\', "\\\\")
        );
        assert!(run_source(&s).is_ok());
        assert!(p.exists());
        let _ = fs::remove_file(p);
    }
}
