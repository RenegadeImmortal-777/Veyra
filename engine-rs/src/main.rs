#![allow(
    clippy::collapsible_if,
    clippy::needless_range_loop,
    clippy::type_complexity,
    clippy::let_and_return,
    clippy::only_used_in_recursion
)]

use std::{
    collections::{HashMap, HashSet},
    env, fs,
    io::{self, Write},
    path::{Component, Path, PathBuf},
    process::{Command, ExitCode},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender, TryRecvError},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::Engine;
use libloading::Library;
use rand::Rng;
use regex::Regex;
#[cfg(feature = "native-services")]
use rusqlite::Connection;
use serde_json::Value as JsonValue;
use sha2::{Digest, Sha256};
use tiny_http::{Header, Response, Server, StatusCode};

const VERSION: &str = "11.0.0-nexus";

mod lexer;

use crate::lexer::{Tok, lex};

mod ast;

use crate::ast::{Expr, Stmt};

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
            let ty = if self.eat(&Tok::Colon) {
                match self.bump() {
                    Tok::Id(s) => Some(s),
                    _ => return Err("expected type name".into()),
                }
            } else {
                None
            };
            self.need(&Tok::Eq, "expected '='")?;
            let e = self.expr()?;
            self.eat(&Tok::Semi);
            return Ok(Stmt::Let(name, ty, e, is_const));
        }
        if self.eat(&Tok::Class) {
            let name = match self.bump() {
                Tok::Id(s) => s,
                x => return Err(format!("expected class name, got {x:?}")),
            };
            let parent = if self.eat(&Tok::Colon) {
                match self.bump() {
                    Tok::Id(s) => Some(s),
                    x => return Err(format!("expected parent class name, got {x:?}")),
                }
            } else {
                None
            };
            let body = self.block()?;
            return Ok(Stmt::Class(name, parent, body));
        }
        if self.eat(&Tok::At) {
            let decorator = match self.bump() {
                Tok::Id(s) => s,
                x => return Err(format!("expected decorator name, got {x:?}")),
            };
            self.need(&Tok::Fn, "expected 'fn' after decorator")?;
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
                    let param_type = if self.eat(&Tok::Colon) {
                        match self.bump() {
                            Tok::Id(s) => Some(s),
                            _ => return Err("expected parameter type".into()),
                        }
                    } else {
                        None
                    };
                    params.push((param, param_type));
                    if self.eat(&Tok::Rp) {
                        break;
                    }
                    self.need(&Tok::Comma, "expected ',' between parameters")?;
                }
            }
            let return_type = if self.eat(&Tok::Arrow) {
                match self.bump() {
                    Tok::Id(s) => Some(s),
                    _ => return Err("expected return type".into()),
                }
            } else {
                None
            };
            let body = self.block()?;
            return Ok(Stmt::DecoratedFn(
                decorator,
                name,
                params,
                return_type,
                body,
            ));
        }
        if self.eat(&Tok::Async) {
            self.need(&Tok::Fn, "expected 'fn' after async")?;
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
                    let param_type = if self.eat(&Tok::Colon) {
                        match self.bump() {
                            Tok::Id(s) => Some(s),
                            _ => return Err("expected parameter type".into()),
                        }
                    } else {
                        None
                    };
                    params.push((param, param_type));
                    if self.eat(&Tok::Rp) {
                        break;
                    }
                    self.need(&Tok::Comma, "expected ',' between parameters")?;
                }
            }
            let return_type = if self.eat(&Tok::Arrow) {
                match self.bump() {
                    Tok::Id(s) => Some(s),
                    _ => return Err("expected return type".into()),
                }
            } else {
                None
            };
            let body = self.block()?;
            return Ok(Stmt::AsyncFn(name, params, return_type, body));
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
                    let param_type = if self.eat(&Tok::Colon) {
                        match self.bump() {
                            Tok::Id(s) => Some(s),
                            _ => return Err("expected parameter type".into()),
                        }
                    } else {
                        None
                    };
                    params.push((param, param_type));
                    if self.eat(&Tok::Rp) {
                        break;
                    }
                    self.need(&Tok::Comma, "expected ',' between parameters")?;
                }
            }
            let return_type = if self.eat(&Tok::Arrow) {
                match self.bump() {
                    Tok::Id(s) => Some(s),
                    _ => return Err("expected return type".into()),
                }
            } else {
                None
            };
            let body = self.block()?;
            return Ok(Stmt::Fn(name, params, return_type, body));
        }
        if self.eat(&Tok::Break) {
            self.eat(&Tok::Semi);
            return Ok(Stmt::Break);
        }
        if self.eat(&Tok::Continue) {
            self.eat(&Tok::Semi);
            return Ok(Stmt::Continue);
        }
        if self.eat(&Tok::Yield) {
            let value = self.expr()?;
            self.eat(&Tok::Semi);
            return Ok(Stmt::Yield(value));
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
            if matches!(self.cur(), Tok::Id(s) if s == "range")
                && self.i + 1 < self.t.len()
                && matches!(self.t[self.i + 1], Tok::Lp)
            {
                self.i += 2;
                let start = self.expr()?;
                self.need(&Tok::Comma, "expected ',' in range")?;
                let end = self.expr()?;
                self.need(&Tok::Rp, "expected ')' after range")?;
                let body = self.block()?;
                return Ok(Stmt::ForRange(name, start, end, body));
            }
            let iterable = self.expr()?;
            let body = self.block()?;
            return Ok(Stmt::ForEach(name, iterable, body));
        }
        if self.eat(&Tok::Try) {
            let body = self.block()?;
            self.need(&Tok::Catch, "expected 'catch' after try block")?;
            let error_name = match self.bump() {
                Tok::Id(s) => s,
                x => return Err(format!("expected catch variable, got {x:?}")),
            };
            let handler = self.block()?;
            let finally = if self.eat(&Tok::Finally) {
                self.block()?
            } else {
                vec![]
            };
            return Ok(Stmt::Try(body, error_name, handler, finally));
        }
        if self.eat(&Tok::Throw) {
            let value = self.expr()?;
            self.eat(&Tok::Semi);
            return Ok(Stmt::Throw(value));
        }
        if self.eat(&Tok::With) {
            let resource = self.expr()?;
            self.need(&Tok::As, "expected 'as' in with statement")?;
            let name = match self.bump() {
                Tok::Id(s) => s,
                x => return Err(format!("expected context variable, got {x:?}")),
            };
            let body = self.block()?;
            return Ok(Stmt::With(resource, name, body));
        }
        if self.eat(&Tok::Match) {
            let subject = self.expr()?;
            self.need(&Tok::Lb, "expected '{' after match expression")?;
            let mut cases = vec![];
            while !self.eat(&Tok::Rb) {
                self.need(&Tok::Case, "expected 'case' in match")?;
                let pattern = if matches!(self.cur(), Tok::Id(s) if s == "_") {
                    self.i += 1;
                    None
                } else {
                    Some(self.expr()?)
                };
                let body = self.block()?;
                cases.push((pattern, body));
            }
            return Ok(Stmt::Match(subject, cases));
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
        if self.eat(&Tok::Eq) {
            let value = self.expr()?;
            self.eat(&Tok::Semi);
            if let Expr::Member(obj, name) = e {
                return Ok(Stmt::SetMember(*obj, name, value));
            }
            return Err("invalid assignment target".into());
        }
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
        if self.eat(&Tok::Await) {
            return Ok(Expr::Await(Box::new(self.unary()?)));
        }
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
        loop {
            if self.eat(&Tok::Ls) {
                let key = self.expr()?;
                self.need(&Tok::Rs, "expected ']' after index")?;
                e = Expr::Index(Box::new(e), Box::new(key));
                continue;
            }
            if self.eat(&Tok::Dot) {
                let name = match self.bump() {
                    Tok::Id(s) => s,
                    x => return Err(format!("expected member name, got {x:?}")),
                };
                if self.eat(&Tok::Lp) {
                    let mut args = vec![];
                    if !self.eat(&Tok::Rp) {
                        loop {
                            args.push(self.expr()?);
                            if self.eat(&Tok::Rp) {
                                break;
                            }
                            self.need(&Tok::Comma, "expected ',' between arguments")?;
                        }
                    }
                    e = Expr::MethodCall(Box::new(e), name, args);
                } else {
                    e = Expr::Member(Box::new(e), name);
                }
                continue;
            }
            break;
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
            Tok::Fn => {
                self.need(&Tok::Lp, "expected '(' after anonymous fn")?;
                let mut params = vec![];
                if !self.eat(&Tok::Rp) {
                    loop {
                        let param = match self.bump() {
                            Tok::Id(s) => s,
                            x => return Err(format!("expected parameter name, got {x:?}")),
                        };
                        let param_type = if self.eat(&Tok::Colon) {
                            match self.bump() {
                                Tok::Id(s) => Some(s),
                                _ => return Err("expected parameter type".into()),
                            }
                        } else {
                            None
                        };
                        params.push((param, param_type));
                        if self.eat(&Tok::Rp) {
                            break;
                        }
                        self.need(&Tok::Comma, "expected ',' between parameters")?;
                    }
                }
                let return_type = if self.eat(&Tok::Arrow) {
                    match self.bump() {
                        Tok::Id(s) => Some(s),
                        _ => return Err("expected return type".into()),
                    }
                } else {
                    None
                };
                let body = self.block()?;
                Ok(Expr::Lambda(params, return_type, body))
            }
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
                if self.eat(&Tok::Rs) {
                    return Ok(Expr::List(vec![]));
                }
                let first = self.expr()?;
                if self.eat(&Tok::For) {
                    let name = match self.bump() {
                        Tok::Id(s) => s,
                        x => return Err(format!("expected comprehension variable, got {x:?}")),
                    };
                    self.need(&Tok::In, "expected 'in' in comprehension")?;
                    let iterable = self.expr()?;
                    let condition = if self.eat(&Tok::If) {
                        Some(Box::new(self.expr()?))
                    } else {
                        None
                    };
                    self.need(&Tok::Rs, "expected ']' after comprehension")?;
                    return Ok(Expr::ListComp(
                        Box::new(first),
                        name,
                        Box::new(iterable),
                        condition,
                    ));
                }
                let mut a = vec![first];
                while !self.eat(&Tok::Rs) {
                    self.need(&Tok::Comma, "expected ','")?;
                    if self.eat(&Tok::Rs) {
                        break;
                    }
                    a.push(self.expr()?);
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
    Object(u64),
    Function(String),
    Generator(u64),
}
fn val_to_json(v: &Val) -> JsonValue {
    match v {
        Val::Num(n) => JsonValue::from(*n),
        Val::Str(s) => JsonValue::from(s.clone()),
        Val::Bool(b) => JsonValue::from(*b),
        Val::Null => JsonValue::Null,
        Val::List(xs) => JsonValue::Array(xs.iter().map(val_to_json).collect()),
        Val::Object(id) => JsonValue::String(format!("<object:{id}>")),
        Val::Function(name) => JsonValue::String(format!("<function:{name}>")),
        Val::Generator(id) => JsonValue::String(format!("<generator:{id}>")),
        Val::Map(pairs) => {
            let mut obj = serde_json::Map::new();
            for (k, v) in pairs {
                obj.insert(k.show(), val_to_json(v));
            }
            JsonValue::Object(obj)
        }
    }
}

#[cfg(feature = "native-services")]
fn val_to_sql_value(v: &Val) -> rusqlite::types::Value {
    match v {
        Val::Null => rusqlite::types::Value::Null,
        Val::Bool(b) => rusqlite::types::Value::Integer(if *b { 1 } else { 0 }),
        Val::Num(n) if n.fract() == 0.0 => rusqlite::types::Value::Integer(*n as i64),
        Val::Num(n) => rusqlite::types::Value::Real(*n),
        Val::Str(s) => rusqlite::types::Value::Text(s.clone()),
        other => rusqlite::types::Value::Text(other.show()),
    }
}

#[cfg(feature = "native-services")]
fn json_stringify_value(v: &Val) -> Result<String, String> {
    serde_json::to_string(&val_to_json(v)).map_err(|e| e.to_string())
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

fn hmac_sha256_bytes(key: &[u8], message: &[u8]) -> Vec<u8> {
    let block_size = 64usize;
    let mut key_block = vec![0u8; block_size];
    if key.len() > block_size {
        let digest = Sha256::digest(key);
        key_block[..digest.len()].copy_from_slice(&digest);
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut ipad = vec![0x36u8; block_size];
    let mut opad = vec![0x5cu8; block_size];
    for i in 0..block_size {
        ipad[i] ^= key_block[i];
        opad[i] ^= key_block[i];
    }
    let mut inner = Sha256::new();
    inner.update(&ipad);
    inner.update(message);
    let inner_digest = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(&opad);
    outer.update(inner_digest);
    outer.finalize().to_vec()
}

fn hmac_sha256_hex(key: &str, message: &str) -> String {
    hmac_sha256_bytes(key.as_bytes(), message.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn pbkdf2_sha256_hex(password: &str, salt: &str, iterations: u32, bytes: usize) -> String {
    let mut out = vec![];
    let mut block_index = 1u32;
    while out.len() < bytes {
        let mut seed = salt.as_bytes().to_vec();
        seed.extend_from_slice(&block_index.to_be_bytes());
        let mut u = hmac_sha256_bytes(password.as_bytes(), &seed);
        let mut t = u.clone();
        for _ in 1..iterations.max(1) {
            u = hmac_sha256_bytes(password.as_bytes(), &u);
            for (x, y) in t.iter_mut().zip(u.iter()) {
                *x ^= *y;
            }
        }
        out.extend_from_slice(&t);
        block_index += 1;
    }
    out.truncate(bytes);
    out.iter().map(|b| format!("{b:02x}")).collect()
}

fn constant_time_strings_equal(a: &str, b: &str) -> bool {
    let aa = a.as_bytes();
    let bb = b.as_bytes();
    let mut diff = aa.len() ^ bb.len();
    let max_len = aa.len().max(bb.len());
    for i in 0..max_len {
        let x = aa.get(i).copied().unwrap_or(0);
        let y = bb.get(i).copied().unwrap_or(0);
        diff |= (x ^ y) as usize;
    }
    diff == 0
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

fn square_matrix(m: &[Vec<f64>]) -> Result<usize, String> {
    let n = m.len();
    if n == 0 || m.iter().any(|r| r.len() != n) {
        return Err("expected non-empty square matrix".into());
    }
    Ok(n)
}

fn matrix_determinant(mut m: Vec<Vec<f64>>) -> Result<f64, String> {
    let n = square_matrix(&m)?;
    let mut det = 1.0;
    let mut sign = 1.0;
    for i in 0..n {
        let mut pivot = i;
        for r in (i + 1)..n {
            if m[r][i].abs() > m[pivot][i].abs() {
                pivot = r;
            }
        }
        if m[pivot][i].abs() < 1e-12 {
            return Ok(0.0);
        }
        if pivot != i {
            m.swap(i, pivot);
            sign *= -1.0;
        }
        let pv = m[i][i];
        det *= pv;
        for r in (i + 1)..n {
            let factor = m[r][i] / pv;
            for c in (i + 1)..n {
                m[r][c] -= factor * m[i][c];
            }
        }
    }
    Ok(det * sign)
}

fn matrix_inverse(mut a: Vec<Vec<f64>>) -> Result<Vec<Vec<f64>>, String> {
    let n = square_matrix(&a)?;
    let mut inv = vec![vec![0.0; n]; n];
    for i in 0..n {
        inv[i][i] = 1.0;
    }
    for i in 0..n {
        let mut pivot = i;
        for r in (i + 1)..n {
            if a[r][i].abs() > a[pivot][i].abs() {
                pivot = r;
            }
        }
        if a[pivot][i].abs() < 1e-12 {
            return Err("matrix is singular".into());
        }
        if pivot != i {
            a.swap(i, pivot);
            inv.swap(i, pivot);
        }
        let pv = a[i][i];
        for c in 0..n {
            a[i][c] /= pv;
            inv[i][c] /= pv;
        }
        for r in 0..n {
            if r == i {
                continue;
            }
            let factor = a[r][i];
            for c in 0..n {
                a[r][c] -= factor * a[i][c];
                inv[r][c] -= factor * inv[i][c];
            }
        }
    }
    Ok(inv)
}

fn solve_linear_system(mut a: Vec<Vec<f64>>, mut b: Vec<f64>) -> Result<Vec<f64>, String> {
    let n = square_matrix(&a)?;
    if b.len() != n {
        return Err("linear solve dimension mismatch".into());
    }
    for i in 0..n {
        let mut pivot = i;
        for r in (i + 1)..n {
            if a[r][i].abs() > a[pivot][i].abs() {
                pivot = r;
            }
        }
        if a[pivot][i].abs() < 1e-12 {
            return Err("matrix is singular".into());
        }
        if pivot != i {
            a.swap(i, pivot);
            b.swap(i, pivot);
        }
        let pv = a[i][i];
        for r in (i + 1)..n {
            let factor = a[r][i] / pv;
            for c in i..n {
                a[r][c] -= factor * a[i][c];
            }
            b[r] -= factor * b[i];
        }
    }
    let mut x = vec![0.0; n];
    for i in (0..n).rev() {
        let rhs = b[i] - ((i + 1)..n).map(|j| a[i][j] * x[j]).sum::<f64>();
        x[i] = rhs / a[i][i];
    }
    Ok(x)
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
            Val::Object(_) | Val::Function(_) | Val::Generator(_) => true,
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
            Val::Object(id) => format!("<object:{id}>"),
            Val::Function(name) => format!("<function:{name}>"),
            Val::Generator(id) => format!("<generator:{id}>"),
        }
    }
}

#[derive(Clone, Debug)]
enum Op {
    Const(Val),
    Load(String),
    Define(String, bool, Option<String>),
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
    GetMember(String),
    SetMember(String),
    Call(String, usize),
    CallMethod(String, usize),
    FunctionRef(String),
    Def(
        String,
        Vec<(String, Option<String>)>,
        Option<String>,
        bool,
        Vec<Op>,
    ),
    DefClass(
        String,
        Option<String>,
        Vec<(
            String,
            Vec<(String, Option<String>)>,
            Option<String>,
            bool,
            Vec<Op>,
        )>,
    ),
    Return,
    Yield,
    PushHandler(usize, String),
    PopHandler,
    Throw,
    JumpIfFalse(usize),
    Jump(usize),
    Halt,
}

#[derive(Clone, Debug)]
struct FunctionDef {
    params: Vec<(String, Option<String>)>,
    return_type: Option<String>,
    is_async: bool,
    code: Vec<Op>,
    captures: HashMap<String, Val>,
    capture_types: HashMap<String, String>,
}

#[derive(Clone, Debug)]
struct ClassDef {
    parent: Option<String>,
    methods: HashMap<String, FunctionDef>,
}

#[derive(Clone, Debug)]
struct ObjectData {
    class_name: String,
    fields: HashMap<String, Val>,
}

static OBJECT_IDS: AtomicU64 = AtomicU64::new(1);
static TASK_IDS: AtomicU64 = AtomicU64::new(1);
static GENERATOR_IDS: AtomicU64 = AtomicU64::new(1);
static CHANNEL_IDS: AtomicU64 = AtomicU64::new(1);
static FUNCTION_IDS: AtomicU64 = AtomicU64::new(1);
static FUNCTION_OBJECTS: OnceLock<Mutex<HashMap<String, FunctionDef>>> = OnceLock::new();
static TENSOR_IDS: AtomicU64 = AtomicU64::new(1);
static TENSORS: OnceLock<Mutex<HashMap<u64, TensorData>>> = OnceLock::new();
static ADAM_STATES: OnceLock<Mutex<HashMap<u64, AdamState>>> = OnceLock::new();

#[derive(Clone, Debug)]
enum TensorOp {
    Leaf,
    Add(u64, u64),
    Sub(u64, u64),
    Mul(u64, u64),
    Div(u64, u64),
    Neg(u64),
    Pow(u64, f64),
    SoftmaxRows(u64, usize, usize),
    Relu(u64),
    Sigmoid(u64),
    Tanh(u64),
    Exp(u64),
    Log(u64),
    Reshape(u64),
    Transpose(u64, usize, usize),
    Sum(u64),
    Mean(u64),
    MatMul(u64, u64, usize, usize, usize),
}

#[derive(Clone, Debug)]
struct TensorData {
    data: Vec<f64>,
    shape: Vec<usize>,
    grad: Vec<f64>,
    requires_grad: bool,
    op: TensorOp,
}

#[derive(Clone, Debug)]
struct AdamState {
    m: Vec<f64>,
    v: Vec<f64>,
    step: u64,
}

fn tensors() -> &'static Mutex<HashMap<u64, TensorData>> {
    TENSORS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn adam_states() -> &'static Mutex<HashMap<u64, AdamState>> {
    ADAM_STATES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn tensor_handle(id: u64, shape: &[usize]) -> Val {
    Val::Map(vec![
        (Val::Str("__tensor_id".into()), Val::Num(id as f64)),
        (
            Val::Str("shape".into()),
            Val::List(shape.iter().map(|n| Val::Num(*n as f64)).collect()),
        ),
    ])
}

fn tensor_id(value: &Val) -> Result<u64, String> {
    let pairs = match value {
        Val::Map(p) => p,
        _ => return Err("expected Tensor handle".into()),
    };
    for (k, v) in pairs {
        if *k == Val::Str("__tensor_id".into()) {
            if let Val::Num(n) = v {
                return Ok(*n as u64);
            }
        }
    }
    Err("invalid Tensor handle".into())
}

fn tensor_from_value(value: &Val, requires_grad: bool) -> Result<Val, String> {
    let (data, shape) = match value {
        Val::List(rows) if rows.iter().all(|x| matches!(x, Val::List(_))) => {
            let matrix = as_matrix(value)?;
            if matrix.is_empty() {
                (vec![], vec![0, 0])
            } else {
                let cols = matrix[0].len();
                if matrix.iter().any(|r| r.len() != cols) {
                    return Err("tensor matrix rows must have equal length".into());
                }
                (
                    matrix.into_iter().flatten().collect(),
                    vec![rows.len(), cols],
                )
            }
        }
        Val::List(_) => {
            let xs = as_num_list(value)?;
            let n = xs.len();
            (xs, vec![n])
        }
        Val::Num(n) => (vec![*n], vec![]),
        _ => return Err("tensor expects Number or numeric List".into()),
    };
    let id = TENSOR_IDS.fetch_add(1, Ordering::Relaxed);
    tensors()
        .lock()
        .map_err(|_| "tensor registry poisoned")?
        .insert(
            id,
            TensorData {
                grad: vec![0.0; data.len()],
                data,
                shape: shape.clone(),
                requires_grad,
                op: TensorOp::Leaf,
            },
        );
    Ok(tensor_handle(id, &shape))
}

fn tensor_snapshot(id: u64) -> Result<TensorData, String> {
    tensors()
        .lock()
        .map_err(|_| "tensor registry poisoned")?
        .get(&id)
        .cloned()
        .ok_or(format!("unknown tensor {id}"))
}

fn tensor_store(
    data: Vec<f64>,
    shape: Vec<usize>,
    requires_grad: bool,
    op: TensorOp,
) -> Result<Val, String> {
    let id = TENSOR_IDS.fetch_add(1, Ordering::Relaxed);
    tensors()
        .lock()
        .map_err(|_| "tensor registry poisoned")?
        .insert(
            id,
            TensorData {
                grad: vec![0.0; data.len()],
                data,
                shape: shape.clone(),
                requires_grad,
                op,
            },
        );
    Ok(tensor_handle(id, &shape))
}

fn tensor_topology(id: u64, seen: &mut HashSet<u64>, order: &mut Vec<u64>) -> Result<(), String> {
    if !seen.insert(id) {
        return Ok(());
    }
    let t = tensor_snapshot(id)?;
    let parents: Vec<u64> = match t.op {
        TensorOp::Leaf => vec![],
        TensorOp::Add(a, b) | TensorOp::Sub(a, b) | TensorOp::Mul(a, b) | TensorOp::Div(a, b) => {
            vec![a, b]
        }
        TensorOp::Neg(a)
        | TensorOp::Pow(a, _)
        | TensorOp::SoftmaxRows(a, _, _)
        | TensorOp::Relu(a)
        | TensorOp::Sigmoid(a)
        | TensorOp::Tanh(a)
        | TensorOp::Exp(a)
        | TensorOp::Log(a)
        | TensorOp::Reshape(a)
        | TensorOp::Transpose(a, _, _)
        | TensorOp::Sum(a)
        | TensorOp::Mean(a) => vec![a],
        TensorOp::MatMul(a, b, _, _, _) => vec![a, b],
    };
    for p in parents {
        tensor_topology(p, seen, order)?;
    }
    order.push(id);
    Ok(())
}

fn tensor_backward(root: u64) -> Result<(), String> {
    let root_tensor = tensor_snapshot(root)?;
    if root_tensor.data.len() != 1 {
        return Err("backward expects a scalar tensor; use tensor_sum or tensor_mean first".into());
    }
    let mut order = vec![];
    tensor_topology(root, &mut HashSet::new(), &mut order)?;
    {
        let mut store = tensors().lock().map_err(|_| "tensor registry poisoned")?;
        for id in &order {
            if let Some(tensor) = store.get_mut(id) {
                for g in &mut tensor.grad {
                    *g = 0.0;
                }
            }
        }
        let t = store.get_mut(&root).ok_or("unknown tensor")?;
        t.grad = vec![1.0];
    }
    for id in order.into_iter().rev() {
        let current = tensor_snapshot(id)?;
        let grad = current.grad.clone();
        match current.op {
            TensorOp::Leaf => {}
            TensorOp::Add(a, b) => {
                let mut store = tensors().lock().map_err(|_| "tensor registry poisoned")?;
                for parent in [a, b] {
                    if let Some(t) = store.get_mut(&parent) {
                        if t.requires_grad {
                            for (dst, src) in t.grad.iter_mut().zip(grad.iter()) {
                                *dst += *src;
                            }
                        }
                    }
                }
            }
            TensorOp::Sub(a, b) => {
                let mut store = tensors().lock().map_err(|_| "tensor registry poisoned")?;
                if let Some(t) = store.get_mut(&a) {
                    if t.requires_grad {
                        for (dst, src) in t.grad.iter_mut().zip(grad.iter()) {
                            *dst += *src;
                        }
                    }
                }
                if let Some(t) = store.get_mut(&b) {
                    if t.requires_grad {
                        for (dst, src) in t.grad.iter_mut().zip(grad.iter()) {
                            *dst -= *src;
                        }
                    }
                }
            }
            TensorOp::Mul(a, b) => {
                let left = tensor_snapshot(a)?;
                let right = tensor_snapshot(b)?;
                let mut store = tensors().lock().map_err(|_| "tensor registry poisoned")?;
                if let Some(t) = store.get_mut(&a) {
                    if t.requires_grad {
                        for i in 0..t.grad.len() {
                            t.grad[i] += grad[i] * right.data[i];
                        }
                    }
                }
                if let Some(t) = store.get_mut(&b) {
                    if t.requires_grad {
                        for i in 0..t.grad.len() {
                            t.grad[i] += grad[i] * left.data[i];
                        }
                    }
                }
            }
            TensorOp::Div(a, b) => {
                let left = tensor_snapshot(a)?;
                let right = tensor_snapshot(b)?;
                let mut store = tensors().lock().map_err(|_| "tensor registry poisoned")?;
                if let Some(t) = store.get_mut(&a) {
                    if t.requires_grad {
                        for i in 0..t.grad.len() {
                            t.grad[i] += grad[i] / right.data[i];
                        }
                    }
                }
                if let Some(t) = store.get_mut(&b) {
                    if t.requires_grad {
                        for i in 0..t.grad.len() {
                            let denom = right.data[i] * right.data[i];
                            t.grad[i] -= grad[i] * left.data[i] / denom;
                        }
                    }
                }
            }
            TensorOp::Neg(a) => {
                let mut store = tensors().lock().map_err(|_| "tensor registry poisoned")?;
                if let Some(t) = store.get_mut(&a) {
                    if t.requires_grad {
                        for (dst, src) in t.grad.iter_mut().zip(grad.iter()) {
                            *dst -= *src;
                        }
                    }
                }
            }
            TensorOp::Pow(a, exponent) => {
                let parent = tensor_snapshot(a)?;
                let mut store = tensors().lock().map_err(|_| "tensor registry poisoned")?;
                if let Some(t) = store.get_mut(&a) {
                    if t.requires_grad {
                        for i in 0..t.grad.len() {
                            t.grad[i] += grad[i] * exponent * parent.data[i].powf(exponent - 1.0);
                        }
                    }
                }
            }
            TensorOp::SoftmaxRows(a, rows, cols) => {
                let mut incoming = vec![0.0; rows * cols];
                for r in 0..rows {
                    let mut dot = 0.0;
                    for c in 0..cols {
                        let idx = r * cols + c;
                        dot += grad[idx] * current.data[idx];
                    }
                    for c in 0..cols {
                        let idx = r * cols + c;
                        incoming[idx] = current.data[idx] * (grad[idx] - dot);
                    }
                }
                let mut store = tensors().lock().map_err(|_| "tensor registry poisoned")?;
                if let Some(t) = store.get_mut(&a) {
                    if t.requires_grad {
                        for (dst, src) in t.grad.iter_mut().zip(incoming) {
                            *dst += src;
                        }
                    }
                }
            }
            TensorOp::Relu(a) => {
                let parent = tensor_snapshot(a)?;
                let mut store = tensors().lock().map_err(|_| "tensor registry poisoned")?;
                if let Some(t) = store.get_mut(&a) {
                    if t.requires_grad {
                        for i in 0..t.grad.len() {
                            t.grad[i] += if parent.data[i] > 0.0 { grad[i] } else { 0.0 };
                        }
                    }
                }
            }
            TensorOp::Sigmoid(a) => {
                let mut store = tensors().lock().map_err(|_| "tensor registry poisoned")?;
                if let Some(t) = store.get_mut(&a) {
                    if t.requires_grad {
                        for i in 0..t.grad.len() {
                            let y = current.data[i];
                            t.grad[i] += grad[i] * y * (1.0 - y);
                        }
                    }
                }
            }
            TensorOp::Tanh(a) => {
                let mut store = tensors().lock().map_err(|_| "tensor registry poisoned")?;
                if let Some(t) = store.get_mut(&a) {
                    if t.requires_grad {
                        for i in 0..t.grad.len() {
                            let y = current.data[i];
                            t.grad[i] += grad[i] * (1.0 - y * y);
                        }
                    }
                }
            }
            TensorOp::Exp(a) => {
                let mut store = tensors().lock().map_err(|_| "tensor registry poisoned")?;
                if let Some(t) = store.get_mut(&a) {
                    if t.requires_grad {
                        for i in 0..t.grad.len() {
                            t.grad[i] += grad[i] * current.data[i];
                        }
                    }
                }
            }
            TensorOp::Log(a) => {
                let parent = tensor_snapshot(a)?;
                let mut store = tensors().lock().map_err(|_| "tensor registry poisoned")?;
                if let Some(t) = store.get_mut(&a) {
                    if t.requires_grad {
                        for i in 0..t.grad.len() {
                            t.grad[i] += grad[i] / parent.data[i];
                        }
                    }
                }
            }
            TensorOp::Reshape(a) => {
                let mut store = tensors().lock().map_err(|_| "tensor registry poisoned")?;
                if let Some(t) = store.get_mut(&a) {
                    if t.requires_grad {
                        for (dst, src) in t.grad.iter_mut().zip(grad.iter()) {
                            *dst += *src;
                        }
                    }
                }
            }
            TensorOp::Transpose(a, rows, cols) => {
                let mut incoming = vec![0.0; rows * cols];
                for r in 0..rows {
                    for c in 0..cols {
                        incoming[r * cols + c] = grad[c * rows + r];
                    }
                }
                let mut store = tensors().lock().map_err(|_| "tensor registry poisoned")?;
                if let Some(t) = store.get_mut(&a) {
                    if t.requires_grad {
                        for (dst, src) in t.grad.iter_mut().zip(incoming) {
                            *dst += src;
                        }
                    }
                }
            }
            TensorOp::Sum(a) => {
                let mut store = tensors().lock().map_err(|_| "tensor registry poisoned")?;
                if let Some(t) = store.get_mut(&a) {
                    if t.requires_grad {
                        for dst in &mut t.grad {
                            *dst += grad[0];
                        }
                    }
                }
            }
            TensorOp::Mean(a) => {
                let mut store = tensors().lock().map_err(|_| "tensor registry poisoned")?;
                if let Some(t) = store.get_mut(&a) {
                    if t.requires_grad && !t.grad.is_empty() {
                        let scale = grad[0] / t.grad.len() as f64;
                        for dst in &mut t.grad {
                            *dst += scale;
                        }
                    }
                }
            }
            TensorOp::MatMul(a, b, m, k, n) => {
                let left = tensor_snapshot(a)?;
                let right = tensor_snapshot(b)?;
                let mut grad_a = vec![0.0; m * k];
                let mut grad_b = vec![0.0; k * n];
                for i in 0..m {
                    for j in 0..n {
                        let g = grad[i * n + j];
                        for p in 0..k {
                            grad_a[i * k + p] += g * right.data[p * n + j];
                            grad_b[p * n + j] += g * left.data[i * k + p];
                        }
                    }
                }
                let mut store = tensors().lock().map_err(|_| "tensor registry poisoned")?;
                if let Some(t) = store.get_mut(&a) {
                    if t.requires_grad {
                        for (dst, src) in t.grad.iter_mut().zip(grad_a) {
                            *dst += src;
                        }
                    }
                }
                if let Some(t) = store.get_mut(&b) {
                    if t.requires_grad {
                        for (dst, src) in t.grad.iter_mut().zip(grad_b) {
                            *dst += src;
                        }
                    }
                }
            }
        }
    }
    Ok(())
}
static TASKS: OnceLock<Mutex<HashMap<u64, thread::JoinHandle<Result<Val, String>>>>> =
    OnceLock::new();
static CHANNELS: OnceLock<Mutex<HashMap<u64, (Sender<Val>, Arc<Mutex<Receiver<Val>>>)>>> =
    OnceLock::new();

fn tasks() -> &'static Mutex<HashMap<u64, thread::JoinHandle<Result<Val, String>>>> {
    TASKS.get_or_init(|| Mutex::new(HashMap::new()))
}
fn channels() -> &'static Mutex<HashMap<u64, (Sender<Val>, Arc<Mutex<Receiver<Val>>>)>> {
    CHANNELS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn function_objects() -> &'static Mutex<HashMap<String, FunctionDef>> {
    FUNCTION_OBJECTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn namespace_module_ops(code: &mut [Op], local_names: &HashSet<String>, prefix: &str) {
    for op in code {
        match op {
            Op::Call(name, _) | Op::FunctionRef(name) => {
                if local_names.contains(name) {
                    *name = format!("{prefix}{name}");
                }
            }
            Op::Def(_, _, _, _, nested) => namespace_module_ops(nested, local_names, prefix),
            Op::DefClass(_, _, methods) => {
                for (_, _, _, _, nested) in methods {
                    namespace_module_ops(nested, local_names, prefix);
                }
            }
            _ => {}
        }
    }
}

fn export_function_object(def: FunctionDef) -> Result<String, String> {
    let id = FUNCTION_IDS.fetch_add(1, Ordering::Relaxed);
    let key = format!("@fn:{id}");
    function_objects()
        .lock()
        .map_err(|_| "function object registry poisoned")?
        .insert(key.clone(), def);
    Ok(key)
}

fn resolve_function_object(name: &str) -> Result<Option<FunctionDef>, String> {
    Ok(function_objects()
        .lock()
        .map_err(|_| "function object registry poisoned")?
        .get(name)
        .cloned())
}

struct LoopCtx {
    breaks: Vec<usize>,
    continues: Vec<usize>,
}

struct Compiler {
    code: Vec<Op>,
    next_temp: usize,
    loops: Vec<LoopCtx>,
}
impl Compiler {
    fn new() -> Self {
        Self {
            code: vec![],
            next_temp: 0,
            loops: vec![],
        }
    }
    fn temp(&mut self, prefix: &str) -> String {
        let n = self.next_temp;
        self.next_temp += 1;
        format!("__{prefix}_{n}")
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
            Expr::Member(obj, name) => {
                self.emit_expr(obj)?;
                self.code.push(Op::GetMember(name.clone()));
            }
            Expr::MethodCall(obj, name, args) => {
                self.emit_expr(obj)?;
                for a in args {
                    self.emit_expr(a)?;
                }
                self.code.push(Op::CallMethod(name.clone(), args.len()));
            }
            Expr::Await(task) => {
                self.emit_expr(task)?;
                self.code.push(Op::Call("await_task".into(), 1));
            }
            Expr::Lambda(params, return_type, body) => {
                let name = self.temp("lambda");
                let mut child = Compiler::new();
                child.emit_stmts(body)?;
                child.code.push(Op::Const(Val::Null));
                child.code.push(Op::Return);
                self.code.push(Op::Def(
                    name.clone(),
                    params.clone(),
                    return_type.clone(),
                    false,
                    child.code,
                ));
                self.code.push(Op::FunctionRef(name));
            }
            Expr::ListComp(value, var, iterable, condition) => {
                let value_fn = self.temp("comp_value");
                let mut value_compiler = Compiler::new();
                value_compiler.emit_expr(value)?;
                value_compiler.code.push(Op::Return);
                self.code.push(Op::Def(
                    value_fn.clone(),
                    vec![(var.clone(), None)],
                    None,
                    false,
                    value_compiler.code,
                ));
                let condition_fn = if let Some(cond) = condition {
                    let name = self.temp("comp_cond");
                    let mut cond_compiler = Compiler::new();
                    cond_compiler.emit_expr(cond)?;
                    cond_compiler.code.push(Op::Return);
                    self.code.push(Op::Def(
                        name.clone(),
                        vec![(var.clone(), None)],
                        Some("Bool".into()),
                        false,
                        cond_compiler.code,
                    ));
                    Some(name)
                } else {
                    None
                };
                let result_name = self.temp("comp_result");
                let iter_name = self.temp("comp_iter");
                let idx_name = self.temp("comp_idx");
                self.code.push(Op::Call("__list".into(), 0));
                self.code.push(Op::Define(result_name.clone(), false, None));
                self.emit_expr(iterable)?;
                self.code.push(Op::Call("__iter_list".into(), 1));
                self.code.push(Op::Define(iter_name.clone(), false, None));
                self.code.push(Op::Const(Val::Num(0.0)));
                self.code
                    .push(Op::Define(idx_name.clone(), false, Some("Int".into())));
                let loop_start = self.code.len();
                self.code.push(Op::Load(idx_name.clone()));
                self.code.push(Op::Load(iter_name.clone()));
                self.code.push(Op::Call("len".into(), 1));
                self.code.push(Op::Lt);
                let end_jump = self.code.len();
                self.code.push(Op::JumpIfFalse(0));
                let mut condition_jump = None;
                if let Some(cond_fn) = condition_fn {
                    self.code.push(Op::Load(iter_name.clone()));
                    self.code.push(Op::Load(idx_name.clone()));
                    self.code.push(Op::Index);
                    self.code.push(Op::Call(cond_fn, 1));
                    let j = self.code.len();
                    self.code.push(Op::JumpIfFalse(0));
                    condition_jump = Some(j);
                }
                self.code.push(Op::Load(result_name.clone()));
                self.code.push(Op::Load(iter_name.clone()));
                self.code.push(Op::Load(idx_name.clone()));
                self.code.push(Op::Index);
                self.code.push(Op::Call(value_fn, 1));
                self.code.push(Op::Call("append".into(), 2));
                self.code.push(Op::Store(result_name.clone()));
                let increment = self.code.len();
                if let Some(j) = condition_jump {
                    self.code[j] = Op::JumpIfFalse(increment);
                }
                self.code.push(Op::Load(idx_name.clone()));
                self.code.push(Op::Const(Val::Num(1.0)));
                self.code.push(Op::Add);
                self.code.push(Op::Store(idx_name.clone()));
                self.code.push(Op::Jump(loop_start));
                let end = self.code.len();
                self.code[end_jump] = Op::JumpIfFalse(end);
                self.code.push(Op::Load(result_name));
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
            Stmt::Let(n, ty, e, c) => {
                self.emit_expr(e)?;
                self.code.push(Op::Define(n.clone(), *c, ty.clone()));
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
                self.loops.push(LoopCtx {
                    breaks: vec![],
                    continues: vec![],
                });
                self.emit_stmts(b)?;
                let ctx = self.loops.pop().unwrap();
                for p in ctx.continues {
                    self.code[p] = Op::Jump(start);
                }
                self.code.push(Op::Jump(start));
                let end = self.code.len();
                self.code[jf] = Op::JumpIfFalse(end);
                for p in ctx.breaks {
                    self.code[p] = Op::Jump(end);
                }
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
                self.loops.push(LoopCtx {
                    breaks: vec![],
                    continues: vec![],
                });
                self.emit_stmts(body)?;
                let ctx = self.loops.pop().unwrap();
                let continue_target = self.code.len();
                for p in ctx.continues {
                    self.code[p] = Op::Jump(continue_target);
                }
                self.code.push(Op::Load(name.clone()));
                self.code.push(Op::Const(Val::Num(1.0)));
                self.code.push(Op::Add);
                self.code.push(Op::Store(name.clone()));
                self.code.push(Op::Jump(loop_start));
                let end = self.code.len();
                self.code[jf] = Op::JumpIfFalse(end);
                for p in ctx.breaks {
                    self.code[p] = Op::Jump(end);
                }
            }
            Stmt::ForEach(name, iterable, body) => {
                let iter_name = self.temp("iter");
                let idx_name = self.temp("idx");
                self.emit_expr(iterable)?;
                self.code.push(Op::Call("__iter_list".into(), 1));
                self.code.push(Op::Define(iter_name.clone(), false, None));
                self.code.push(Op::Const(Val::Num(0.0)));
                self.code.push(Op::Define(idx_name.clone(), false, None));
                let loop_start = self.code.len();
                self.code.push(Op::Load(idx_name.clone()));
                self.code.push(Op::Load(iter_name.clone()));
                self.code.push(Op::Call("len".into(), 1));
                self.code.push(Op::Lt);
                let jf = self.code.len();
                self.code.push(Op::JumpIfFalse(0));
                self.code.push(Op::Load(iter_name.clone()));
                self.code.push(Op::Load(idx_name.clone()));
                self.code.push(Op::Index);
                self.code.push(Op::Store(name.clone()));
                self.loops.push(LoopCtx {
                    breaks: vec![],
                    continues: vec![],
                });
                self.emit_stmts(body)?;
                let ctx = self.loops.pop().unwrap();
                let continue_target = self.code.len();
                for p in ctx.continues {
                    self.code[p] = Op::Jump(continue_target);
                }
                self.code.push(Op::Load(idx_name.clone()));
                self.code.push(Op::Const(Val::Num(1.0)));
                self.code.push(Op::Add);
                self.code.push(Op::Store(idx_name));
                self.code.push(Op::Jump(loop_start));
                let end = self.code.len();
                self.code[jf] = Op::JumpIfFalse(end);
                for p in ctx.breaks {
                    self.code[p] = Op::Jump(end);
                }
            }
            Stmt::Match(subject, cases) => {
                let subject_name = self.temp("match_subject");
                self.emit_expr(subject)?;
                self.code
                    .push(Op::Define(subject_name.clone(), false, None));
                let mut end_jumps = vec![];
                let mut wildcard_seen = false;
                for (pattern, body) in cases {
                    if wildcard_seen {
                        return Err("match cases after wildcard '_' are unreachable".into());
                    }
                    if let Some(pattern) = pattern {
                        self.code.push(Op::Load(subject_name.clone()));
                        self.emit_expr(pattern)?;
                        self.code.push(Op::Eq);
                        let next_case = self.code.len();
                        self.code.push(Op::JumpIfFalse(0));
                        self.emit_stmts(body)?;
                        let end_jump = self.code.len();
                        self.code.push(Op::Jump(0));
                        end_jumps.push(end_jump);
                        let next = self.code.len();
                        self.code[next_case] = Op::JumpIfFalse(next);
                    } else {
                        wildcard_seen = true;
                        self.emit_stmts(body)?;
                        let end_jump = self.code.len();
                        self.code.push(Op::Jump(0));
                        end_jumps.push(end_jump);
                    }
                }
                let end = self.code.len();
                for j in end_jumps {
                    self.code[j] = Op::Jump(end);
                }
            }
            Stmt::With(resource, name, body) => {
                let resource_name = self.temp("with_resource");
                self.emit_expr(resource)?;
                self.code
                    .push(Op::Define(resource_name.clone(), false, None));
                self.code.push(Op::Load(resource_name.clone()));
                self.code.push(Op::CallMethod("enter".into(), 0));
                self.code.push(Op::Define(name.clone(), false, None));
                self.emit_stmts(body)?;
                self.code.push(Op::Load(resource_name));
                self.code.push(Op::CallMethod("exit".into(), 0));
                self.code.push(Op::Pop);
            }
            Stmt::Try(body, error_name, handler, finally) => {
                let push = self.code.len();
                self.code.push(Op::PushHandler(0, error_name.clone()));
                self.emit_stmts(body)?;
                self.code.push(Op::PopHandler);
                let jump_to_finally = self.code.len();
                self.code.push(Op::Jump(0));
                let handler_start = self.code.len();
                self.code[push] = Op::PushHandler(handler_start, error_name.clone());
                self.emit_stmts(handler)?;
                let finally_start = self.code.len();
                self.code[jump_to_finally] = Op::Jump(finally_start);
                self.emit_stmts(finally)?;
            }
            Stmt::Throw(value) => {
                self.emit_expr(value)?;
                self.code.push(Op::Throw);
            }
            Stmt::SetMember(obj, name, value) => {
                self.emit_expr(obj)?;
                self.emit_expr(value)?;
                self.code.push(Op::SetMember(name.clone()));
            }
            Stmt::Class(name, parent, body) => {
                let mut methods = vec![];
                for item in body {
                    match item {
                        Stmt::Fn(method, params, return_type, statements) => {
                            let mut child = Compiler::new();
                            child.emit_stmts(statements)?;
                            child.code.push(Op::Const(Val::Null));
                            child.code.push(Op::Return);
                            methods.push((
                                method.clone(),
                                params.clone(),
                                return_type.clone(),
                                false,
                                child.code,
                            ));
                        }
                        Stmt::AsyncFn(method, params, return_type, statements) => {
                            let mut child = Compiler::new();
                            child.emit_stmts(statements)?;
                            child.code.push(Op::Const(Val::Null));
                            child.code.push(Op::Return);
                            methods.push((
                                method.clone(),
                                params.clone(),
                                return_type.clone(),
                                true,
                                child.code,
                            ));
                        }
                        _ => return Err("class bodies currently contain methods only".into()),
                    }
                }
                self.code
                    .push(Op::DefClass(name.clone(), parent.clone(), methods));
            }
            Stmt::Fn(name, params, return_type, body) => {
                let mut child = Compiler::new();
                child.emit_stmts(body)?;
                child.code.push(Op::Const(Val::Null));
                child.code.push(Op::Return);
                self.code.push(Op::Def(
                    name.clone(),
                    params.clone(),
                    return_type.clone(),
                    false,
                    child.code,
                ));
            }
            Stmt::DecoratedFn(decorator, name, params, return_type, body) => {
                let mut child = Compiler::new();
                child.emit_stmts(body)?;
                child.code.push(Op::Const(Val::Null));
                child.code.push(Op::Return);
                self.code.push(Op::Def(
                    name.clone(),
                    params.clone(),
                    return_type.clone(),
                    false,
                    child.code,
                ));
                self.code.push(Op::FunctionRef(name.clone()));
                self.code.push(Op::Call(decorator.clone(), 1));
                self.code
                    .push(Op::Define(name.clone(), false, Some("Function".into())));
            }
            Stmt::AsyncFn(name, params, return_type, body) => {
                let mut child = Compiler::new();
                child.emit_stmts(body)?;
                child.code.push(Op::Const(Val::Null));
                child.code.push(Op::Return);
                self.code.push(Op::Def(
                    name.clone(),
                    params.clone(),
                    return_type.clone(),
                    true,
                    child.code,
                ));
            }
            Stmt::Yield(value) => {
                self.emit_expr(value)?;
                self.code.push(Op::Yield);
            }
            Stmt::Return(value) => {
                if let Some(expr) = value {
                    self.emit_expr(expr)?;
                } else {
                    self.code.push(Op::Const(Val::Null));
                }
                self.code.push(Op::Return);
            }
            Stmt::Break => {
                let p = self.code.len();
                self.code.push(Op::Jump(0));
                let ctx = self.loops.last_mut().ok_or("'break' used outside loop")?;
                ctx.breaks.push(p);
            }
            Stmt::Continue => {
                let p = self.code.len();
                self.code.push(Op::Jump(0));
                let ctx = self
                    .loops
                    .last_mut()
                    .ok_or("'continue' used outside loop")?;
                ctx.continues.push(p);
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
    var_types: HashMap<String, String>,
    consts: HashSet<String>,
    funcs: HashMap<String, FunctionDef>,
    classes: HashMap<String, ClassDef>,
    objects: Arc<Mutex<HashMap<u64, ObjectData>>>,
    handlers: Vec<(usize, String)>,
    trace: bool,
    finished: bool,
}

static GENERATORS: OnceLock<Mutex<HashMap<u64, Vm>>> = OnceLock::new();

fn generators() -> &'static Mutex<HashMap<u64, Vm>> {
    GENERATORS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn resume_generator(id: u64) -> Result<Option<Val>, String> {
    let mut vm = generators()
        .lock()
        .map_err(|_| "generator registry poisoned")?
        .remove(&id)
        .ok_or(format!("unknown or completed generator {id}"))?;
    if vm.finished {
        return Ok(None);
    }
    let value = vm.execute()?;
    if vm.finished {
        Ok(None)
    } else {
        generators()
            .lock()
            .map_err(|_| "generator registry poisoned")?
            .insert(id, vm);
        Ok(Some(value))
    }
}

fn collect_generator(id: u64) -> Result<Vec<Val>, String> {
    let mut out = vec![];
    loop {
        match resume_generator(id) {
            Ok(Some(v)) => out.push(v),
            Ok(None) => break,
            Err(e) if e.starts_with("unknown or completed generator") => break,
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

impl Vm {
    fn new(code: Vec<Op>) -> Self {
        Self {
            code,
            ip: 0,
            stack: vec![],
            vars: HashMap::new(),
            var_types: HashMap::new(),
            consts: HashSet::new(),
            funcs: HashMap::new(),
            classes: HashMap::new(),
            objects: Arc::new(Mutex::new(HashMap::new())),
            handlers: vec![],
            trace: false,
            finished: false,
        }
    }
    fn nested(
        code: Vec<Op>,
        vars: HashMap<String, Val>,
        funcs: HashMap<String, FunctionDef>,
        classes: HashMap<String, ClassDef>,
        objects: Arc<Mutex<HashMap<u64, ObjectData>>>,
    ) -> Self {
        Self {
            code,
            ip: 0,
            stack: vec![],
            vars,
            var_types: HashMap::new(),
            consts: HashSet::new(),
            funcs,
            classes,
            objects,
            handlers: vec![],
            trace: false,
            finished: false,
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
    fn class_is_a(&self, class_name: &str, target: &str) -> bool {
        let mut current = Some(class_name.to_string());
        while let Some(name) = current {
            if name == target {
                return true;
            }
            current = self.classes.get(&name).and_then(|c| c.parent.clone());
        }
        false
    }
    fn value_matches(&self, value: &Val, ty: &str) -> bool {
        match ty {
            "Any" => true,
            "Number" | "Float" => matches!(value, Val::Num(_)),
            "Int" => matches!(value,Val::Num(n) if n.fract()==0.0),
            "String" => matches!(value, Val::Str(_)),
            "Bool" => matches!(value, Val::Bool(_)),
            "List" => matches!(value, Val::List(_)),
            "Map" => matches!(value, Val::Map(_)),
            "Null" | "Void" => matches!(value, Val::Null),
            "Object" => matches!(value, Val::Object(_)),
            "Function" => matches!(value, Val::Function(_)),
            "Generator" => matches!(value, Val::Generator(_)),
            custom => match value {
                Val::Object(id) => {
                    let Ok(heap) = self.objects.lock() else {
                        return false;
                    };
                    heap.get(id)
                        .map(|o| self.class_is_a(&o.class_name, custom))
                        .unwrap_or(false)
                }
                _ => false,
            },
        }
    }
    fn find_method(&self, class_name: &str, method: &str) -> Option<FunctionDef> {
        let mut current = Some(class_name.to_string());
        while let Some(name) = current {
            let class = self.classes.get(&name)?;
            if let Some(def) = class.methods.get(method) {
                return Some(def.clone());
            }
            current = class.parent.clone();
        }
        None
    }
    fn call_def(&self, def: FunctionDef, args: Vec<Val>) -> Result<Val, String> {
        if def.params.len() != args.len() {
            return Err(format!(
                "function expects {} arguments, got {}",
                def.params.len(),
                args.len()
            ));
        }
        let is_generator = def.code.iter().any(|op| matches!(op, Op::Yield));
        let is_async = def.is_async;
        if is_async && is_generator {
            return Err("async generator functions are not yet supported".into());
        }
        let mut vars = def.captures.clone();
        let mut var_types = def.capture_types.clone();
        let return_type = def.return_type.clone();
        for ((param, ty), value) in def.params.into_iter().zip(args.into_iter()) {
            if let Some(ref expected) = ty {
                if !self.value_matches(&value, expected) {
                    return Err(format!(
                        "argument '{param}' expected {expected}, got {}",
                        value.show()
                    ));
                }
                var_types.insert(param.clone(), expected.clone());
            }
            vars.insert(param, value);
        }
        let mut nested = Vm::nested(
            def.code,
            vars,
            self.funcs.clone(),
            self.classes.clone(),
            self.objects.clone(),
        );
        nested.var_types = var_types;
        if is_async {
            let id = TASK_IDS.fetch_add(1, Ordering::Relaxed);
            let expected_return = return_type.clone();
            let handle = thread::spawn(move || {
                let result = nested.execute()?;
                if let Some(expected) = expected_return {
                    if !nested.value_matches(&result, &expected) {
                        return Err(format!(
                            "async return value expected {expected}, got {}",
                            result.show()
                        ));
                    }
                }
                Ok(result)
            });
            tasks()
                .lock()
                .map_err(|_| "task registry poisoned")?
                .insert(id, handle);
            return Ok(Val::Num(id as f64));
        }
        if is_generator {
            if let Some(expected) = return_type {
                if expected != "Generator" && expected != "Any" {
                    return Err(format!(
                        "generator function return type must be Generator or Any, got {expected}"
                    ));
                }
            }
            let id = GENERATOR_IDS.fetch_add(1, Ordering::Relaxed);
            generators()
                .lock()
                .map_err(|_| "generator registry poisoned")?
                .insert(id, nested);
            return Ok(Val::Generator(id));
        }
        let result = nested.execute()?;
        if let Some(expected) = return_type {
            if !self.value_matches(&result, &expected) {
                return Err(format!(
                    "return value expected {expected}, got {}",
                    result.show()
                ));
            }
        }
        Ok(result)
    }
    fn instantiate(&self, class_name: &str, args: Vec<Val>) -> Result<Val, String> {
        if !self.classes.contains_key(class_name) {
            return Err(format!("unknown class '{class_name}'"));
        }
        let id = OBJECT_IDS.fetch_add(1, Ordering::Relaxed);
        self.objects
            .lock()
            .map_err(|_| "object heap poisoned")?
            .insert(
                id,
                ObjectData {
                    class_name: class_name.to_string(),
                    fields: HashMap::new(),
                },
            );
        if let Some(init) = self.find_method(class_name, "init") {
            let mut call_args = vec![Val::Object(id)];
            call_args.extend(args);
            let _ = self.call_def(init, call_args)?;
        } else if !args.is_empty() {
            return Err(format!("{class_name} has no init method"));
        }
        Ok(Val::Object(id))
    }
    fn callable_def(&self, value: &Val) -> Result<(String, FunctionDef), String> {
        let name = match value {
            Val::Str(s) | Val::Function(s) => s.clone(),
            _ => return Err("callable must be a function value or function name String".into()),
        };
        let def = if let Some(def) = self.funcs.get(&name).cloned() {
            def
        } else if let Some(def) = resolve_function_object(&name)? {
            def
        } else {
            return Err(format!("unknown function '{name}'"));
        };
        Ok((name, def))
    }
    fn invoke(&self, name: &str, args: Vec<Val>) -> Result<Val, String> {
        let def = self
            .funcs
            .get(name)
            .cloned()
            .ok_or(format!("unknown function '{name}'"))?;
        self.call_def(def, args)
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
            if self.trace {
                let mut vars = self
                    .vars
                    .iter()
                    .map(|(k, v)| format!("{k}={}", v.show()))
                    .collect::<Vec<_>>();
                vars.sort();
                eprintln!(
                    "[veyra-debug pc={:04}] {:?} | stack=[{}] | vars={{ {} }}",
                    self.ip,
                    op,
                    self.stack
                        .iter()
                        .map(Val::show)
                        .collect::<Vec<_>>()
                        .join(", "),
                    vars.join(", ")
                );
            }
            self.ip += 1;
            match op {
                Op::Const(v) => self.stack.push(v),
                Op::Load(n) => {
                    let value = if let Some(v) = self.vars.get(&n) {
                        v.clone()
                    } else if self.funcs.contains_key(&n) {
                        Val::Function(n.clone())
                    } else {
                        return Err(format!("undefined name '{n}'"));
                    };
                    self.stack.push(value);
                }
                Op::Define(n, is_const, ty) => {
                    let v = self.pop()?;
                    if self.vars.contains_key(&n) && self.consts.contains(&n) {
                        return Err(format!("cannot redefine constant '{n}'"));
                    }
                    if self.vars.contains_key(&n) && is_const {
                        return Err(format!("name '{n}' already defined"));
                    }
                    if let Some(ref expected) = ty {
                        if !self.value_matches(&v, expected) {
                            return Err(format!(
                                "declaration '{n}' expected {expected}, got {}",
                                v.show()
                            ));
                        }
                        self.var_types.insert(n.clone(), expected.clone());
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
                    if let Some(expected) = self.var_types.get(&n).cloned() {
                        if !self.value_matches(&v, &expected) {
                            return Err(format!(
                                "assignment '{n}' expected {expected}, got {}",
                                v.show()
                            ));
                        }
                    }
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
                Op::GetMember(name) => {
                    let obj = self.pop()?;
                    let value = match obj {
                        Val::Object(id) => {
                            let heap = self.objects.lock().map_err(|_| "object heap poisoned")?;
                            let data = heap.get(&id).ok_or("dangling object")?;
                            data.fields
                                .get(&name)
                                .cloned()
                                .ok_or(format!("undefined field '{name}'"))?
                        }
                        Val::Map(pairs) => pairs
                            .iter()
                            .find(|(k, _)| *k == Val::Str(name.clone()))
                            .map(|(_, v)| v.clone())
                            .ok_or(format!("undefined key '{name}'"))?,
                        _ => return Err("member access requires object or map".into()),
                    };
                    self.stack.push(value);
                }
                Op::SetMember(name) => {
                    let value = self.pop()?;
                    let obj = self.pop()?;
                    match obj {
                        Val::Object(id) => {
                            let mut heap =
                                self.objects.lock().map_err(|_| "object heap poisoned")?;
                            let data = heap.get_mut(&id).ok_or("dangling object")?;
                            data.fields.insert(name, value);
                        }
                        _ => return Err("member assignment requires object".into()),
                    }
                }
                Op::CallMethod(name, k) => {
                    let mut args = vec![];
                    for _ in 0..k {
                        args.push(self.pop()?);
                    }
                    args.reverse();
                    let receiver = self.pop()?;
                    let value = match receiver {
                        Val::Object(id) => {
                            let class_name = {
                                let heap =
                                    self.objects.lock().map_err(|_| "object heap poisoned")?;
                                heap.get(&id).ok_or("dangling object")?.class_name.clone()
                            };
                            let method = self
                                .find_method(&class_name, &name)
                                .ok_or(format!("undefined method '{name}' on {class_name}"))?;
                            let mut call_args = vec![Val::Object(id)];
                            call_args.extend(args);
                            self.call_def(method, call_args)?
                        }
                        Val::Map(pairs) => {
                            let member = pairs
                                .iter()
                                .find(|(k, _)| *k == Val::Str(name.clone()))
                                .map(|(_, v)| v.clone())
                                .ok_or(format!("undefined module member '{name}'"))?;
                            let (_, def) = self.callable_def(&member)?;
                            self.call_def(def, args)?
                        }
                        _ => return Err("method call requires object or module map".into()),
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
                        "import_package" => {
                            if a.len() != 1 {
                                return Err("import_package expects package name".into());
                            }
                            let package = match &a[0] {
                                Val::Str(s) => s.clone(),
                                _ => return Err("package name must be String".into()),
                            };
                            let cwd = env::current_dir().map_err(|e| e.to_string())?;
                            let root = project_root(&cwd)?;
                            let manifest: JsonValue = serde_json::from_str(
                                &fs::read_to_string(root.join("veyra.json"))
                                    .map_err(|e| e.to_string())?,
                            )
                            .map_err(|e| e.to_string())?;
                            let meta = manifest
                                .get("packages")
                                .and_then(|p| p.get(&package))
                                .ok_or(format!("package '{package}' is not installed"))?;
                            let entry = meta
                                .get("entry")
                                .and_then(JsonValue::as_str)
                                .unwrap_or("main.vy");
                            let path = root.join("vendor").join(&package).join(entry);
                            let source = load_source(path.to_string_lossy().as_ref())?;
                            let mut module_vm = Vm::new(compile_source(&source)?);
                            module_vm.run()?;
                            let local_names: HashSet<String> =
                                module_vm.funcs.keys().cloned().collect();
                            let module_id = FUNCTION_IDS.fetch_add(1, Ordering::Relaxed);
                            let prefix = format!("@module:{package}:{module_id}:");
                            let mut exports = vec![];
                            for (name, mut def) in module_vm.funcs {
                                namespace_module_ops(&mut def.code, &local_names, &prefix);
                                for value in def.captures.values_mut() {
                                    if let Val::Function(target) = value {
                                        if local_names.contains(target) {
                                            *target = format!("{prefix}{target}");
                                        }
                                    }
                                }
                                let qualified = format!("{prefix}{name}");
                                self.funcs.insert(qualified.clone(), def);
                                exports.push((Val::Str(name), Val::Function(qualified)));
                            }
                            for (name, value) in module_vm.vars {
                                if !name.starts_with("__") {
                                    exports.push((Val::Str(name), value));
                                }
                            }
                            Val::Map(exports)
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
                        "type_of" => {
                            if a.len() != 1 {
                                return Err("type_of expects 1 argument".into());
                            }
                            let name = match &a[0] {
                                Val::Num(n) => {
                                    if n.fract() == 0.0 {
                                        "Int".to_string()
                                    } else {
                                        "Number".to_string()
                                    }
                                }
                                Val::Str(_) => "String".to_string(),
                                Val::Bool(_) => "Bool".to_string(),
                                Val::Null => "Null".to_string(),
                                Val::List(_) => "List".to_string(),
                                Val::Map(_) => "Map".to_string(),
                                Val::Function(_) => "Function".to_string(),
                                Val::Generator(_) => "Generator".to_string(),
                                Val::Object(id) => {
                                    let heap =
                                        self.objects.lock().map_err(|_| "object heap poisoned")?;
                                    heap.get(id)
                                        .map(|o| o.class_name.clone())
                                        .unwrap_or_else(|| "Object".into())
                                }
                            };
                            Val::Str(name)
                        }
                        "is_instance" => {
                            if a.len() != 2 {
                                return Err("is_instance expects value,type".into());
                            }
                            let ty = match &a[1] {
                                Val::Str(s) => s,
                                _ => return Err("type must be String".into()),
                            };
                            Val::Bool(self.value_matches(&a[0], ty))
                        }
                        "has_field" => {
                            if a.len() != 2 {
                                return Err("has_field expects object,name".into());
                            }
                            let id = match a[0] {
                                Val::Object(id) => id,
                                _ => return Err("has_field expects Object".into()),
                            };
                            let name = match &a[1] {
                                Val::Str(s) => s,
                                _ => return Err("field name must be String".into()),
                            };
                            let heap = self.objects.lock().map_err(|_| "object heap poisoned")?;
                            Val::Bool(
                                heap.get(&id)
                                    .map(|o| o.fields.contains_key(name))
                                    .unwrap_or(false),
                            )
                        }
                        "int" => {
                            if a.len() != 1 {
                                return Err("int expects 1 argument".into());
                            }
                            Val::Num(match &a[0] {
                                Val::Num(n) => n.trunc(),
                                Val::Bool(b) => {
                                    if *b {
                                        1.0
                                    } else {
                                        0.0
                                    }
                                }
                                Val::Str(s) => s
                                    .trim()
                                    .parse::<f64>()
                                    .map_err(|_| "int could not parse String")?
                                    .trunc(),
                                _ => return Err("int supports Number/Bool/String".into()),
                            })
                        }
                        "float" => {
                            if a.len() != 1 {
                                return Err("float expects 1 argument".into());
                            }
                            Val::Num(match &a[0] {
                                Val::Num(n) => *n,
                                Val::Bool(b) => {
                                    if *b {
                                        1.0
                                    } else {
                                        0.0
                                    }
                                }
                                Val::Str(s) => s
                                    .trim()
                                    .parse::<f64>()
                                    .map_err(|_| "float could not parse String")?,
                                _ => return Err("float supports Number/Bool/String".into()),
                            })
                        }
                        "bool" => {
                            if a.len() != 1 {
                                return Err("bool expects 1 argument".into());
                            }
                            Val::Bool(a[0].truthy())
                        }
                        "assert" => {
                            if a.is_empty() || a.len() > 2 {
                                return Err("assert expects condition[,message]".into());
                            }
                            if !a[0].truthy() {
                                let message = if a.len() == 2 {
                                    a[1].show()
                                } else {
                                    "assertion failed".into()
                                };
                                return Err(message);
                            }
                            Val::Null
                        }
                        "sum" => {
                            if a.len() != 1 {
                                return Err("sum expects list".into());
                            }
                            Val::Num(as_num_list(&a[0])?.iter().sum())
                        }
                        "min" => {
                            if a.len() != 1 {
                                return Err("min expects list".into());
                            }
                            let xs = as_num_list(&a[0])?;
                            Val::Num(
                                xs.into_iter()
                                    .reduce(f64::min)
                                    .ok_or("min requires non-empty list")?,
                            )
                        }
                        "max" => {
                            if a.len() != 1 {
                                return Err("max expects list".into());
                            }
                            let xs = as_num_list(&a[0])?;
                            Val::Num(
                                xs.into_iter()
                                    .reduce(f64::max)
                                    .ok_or("max requires non-empty list")?,
                            )
                        }
                        "all" => {
                            if a.len() != 1 {
                                return Err("all expects list".into());
                            }
                            let xs = match &a[0] {
                                Val::List(v) => v,
                                _ => return Err("all expects List".into()),
                            };
                            Val::Bool(xs.iter().all(Val::truthy))
                        }
                        "any" => {
                            if a.len() != 1 {
                                return Err("any expects list".into());
                            }
                            let xs = match &a[0] {
                                Val::List(v) => v,
                                _ => return Err("any expects List".into()),
                            };
                            Val::Bool(xs.iter().any(Val::truthy))
                        }
                        "__list" => Val::List(a),
                        "next" => {
                            if a.len() != 1 {
                                return Err("next expects generator".into());
                            }
                            let id = match a[0] {
                                Val::Generator(id) => id,
                                _ => return Err("next expects Generator".into()),
                            };
                            resume_generator(id)?.unwrap_or(Val::Null)
                        }
                        "generator_done" => {
                            if a.len() != 1 {
                                return Err("generator_done expects generator".into());
                            }
                            let id = match a[0] {
                                Val::Generator(id) => id,
                                _ => return Err("generator_done expects Generator".into()),
                            };
                            Val::Bool(
                                !generators()
                                    .lock()
                                    .map_err(|_| "generator registry poisoned")?
                                    .contains_key(&id),
                            )
                        }
                        "collect" => {
                            if a.len() != 1 {
                                return Err("collect expects iterable".into());
                            }
                            match &a[0] {
                                Val::Generator(id) => Val::List(collect_generator(*id)?),
                                Val::List(v) => Val::List(v.clone()),
                                Val::Str(s) => {
                                    Val::List(s.chars().map(|c| Val::Str(c.to_string())).collect())
                                }
                                Val::Map(pairs) => {
                                    Val::List(pairs.iter().map(|(k, _)| k.clone()).collect())
                                }
                                _ => return Err("collect expects Generator/List/String/Map".into()),
                            }
                        }
                        "__iter_list" => {
                            if a.len() != 1 {
                                return Err("__iter_list expects 1 argument".into());
                            }
                            match &a[0] {
                                Val::List(v) => Val::List(v.clone()),
                                Val::Str(s) => {
                                    Val::List(s.chars().map(|c| Val::Str(c.to_string())).collect())
                                }
                                Val::Map(pairs) => {
                                    Val::List(pairs.iter().map(|(k, _)| k.clone()).collect())
                                }
                                Val::Generator(id) => Val::List(collect_generator(*id)?),
                                _ => return Err("value is not iterable".into()),
                            }
                        }
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
                        "tuple" => {
                            if a.len() != 1 {
                                return Err("tuple expects list".into());
                            }
                            match &a[0] {
                                Val::List(v) => Val::List(v.clone()),
                                _ => return Err("tuple expects List".into()),
                            }
                        }
                        "set" => {
                            if a.len() != 1 {
                                return Err("set expects list".into());
                            }
                            let xs = match &a[0] {
                                Val::List(v) => v,
                                _ => return Err("set expects List".into()),
                            };
                            let mut out = vec![];
                            for x in xs {
                                if !out.contains(x) {
                                    out.push(x.clone());
                                }
                            }
                            Val::List(out)
                        }
                        "set_union" => {
                            if a.len() != 2 {
                                return Err("set_union expects two lists".into());
                            }
                            let x = match &a[0] {
                                Val::List(v) => v,
                                _ => return Err("set_union expects Lists".into()),
                            };
                            let y = match &a[1] {
                                Val::List(v) => v,
                                _ => return Err("set_union expects Lists".into()),
                            };
                            let mut out = vec![];
                            for v in x.iter().chain(y.iter()) {
                                if !out.contains(v) {
                                    out.push(v.clone());
                                }
                            }
                            Val::List(out)
                        }
                        "set_intersection" => {
                            if a.len() != 2 {
                                return Err("set_intersection expects two lists".into());
                            }
                            let x = match &a[0] {
                                Val::List(v) => v,
                                _ => return Err("set_intersection expects Lists".into()),
                            };
                            let y = match &a[1] {
                                Val::List(v) => v,
                                _ => return Err("set_intersection expects Lists".into()),
                            };
                            let mut out = vec![];
                            for v in x {
                                if y.contains(v) && !out.contains(v) {
                                    out.push(v.clone());
                                }
                            }
                            Val::List(out)
                        }
                        "set_difference" => {
                            if a.len() != 2 {
                                return Err("set_difference expects two lists".into());
                            }
                            let x = match &a[0] {
                                Val::List(v) => v,
                                _ => return Err("set_difference expects Lists".into()),
                            };
                            let y = match &a[1] {
                                Val::List(v) => v,
                                _ => return Err("set_difference expects Lists".into()),
                            };
                            Val::List(x.iter().filter(|v| !y.contains(v)).cloned().collect())
                        }
                        "contains" => {
                            if a.len() != 2 {
                                return Err("contains expects collection,value".into());
                            }
                            Val::Bool(match &a[0] {
                                Val::List(v) => v.contains(&a[1]),
                                Val::Str(s) => match &a[1] {
                                    Val::Str(x) => s.contains(x),
                                    _ => false,
                                },
                                Val::Map(p) => p.iter().any(|(k, _)| *k == a[1]),
                                _ => return Err("contains supports List/String/Map".into()),
                            })
                        }
                        "index_of" => {
                            if a.len() != 2 {
                                return Err("index_of expects collection,value".into());
                            }
                            match &a[0] {
                                Val::List(v) => v
                                    .iter()
                                    .position(|x| x == &a[1])
                                    .map(|i| Val::Num(i as f64))
                                    .unwrap_or(Val::Null),
                                Val::Str(s) => match &a[1] {
                                    Val::Str(x) => {
                                        s.find(x).map(|i| Val::Num(i as f64)).unwrap_or(Val::Null)
                                    }
                                    _ => Val::Null,
                                },
                                _ => return Err("index_of supports List/String".into()),
                            }
                        }
                        "append" => {
                            if a.len() != 2 {
                                return Err("append expects list,value".into());
                            }
                            let mut xs = match &a[0] {
                                Val::List(v) => v.clone(),
                                _ => return Err("append expects List".into()),
                            };
                            xs.push(a[1].clone());
                            Val::List(xs)
                        }
                        "concat" => {
                            if a.len() != 2 {
                                return Err("concat expects two lists".into());
                            }
                            let mut x = match &a[0] {
                                Val::List(v) => v.clone(),
                                _ => return Err("concat expects Lists".into()),
                            };
                            let y = match &a[1] {
                                Val::List(v) => v.clone(),
                                _ => return Err("concat expects Lists".into()),
                            };
                            x.extend(y);
                            Val::List(x)
                        }
                        "slice" => {
                            if a.len() != 3 {
                                return Err("slice expects value,start,end".into());
                            }
                            let start = match a[1] {
                                Val::Num(n) => n.max(0.0) as usize,
                                _ => return Err("start must be Number".into()),
                            };
                            let end = match a[2] {
                                Val::Num(n) => n.max(0.0) as usize,
                                _ => return Err("end must be Number".into()),
                            };
                            match &a[0] {
                                Val::List(v) => Val::List(
                                    v[start.min(v.len())..end.min(v.len()).max(start.min(v.len()))]
                                        .to_vec(),
                                ),
                                Val::Str(s) => {
                                    let chars: Vec<char> = s.chars().collect();
                                    Val::Str(
                                        chars[start.min(chars.len())
                                            ..end.min(chars.len()).max(start.min(chars.len()))]
                                            .iter()
                                            .collect(),
                                    )
                                }
                                _ => return Err("slice supports List/String".into()),
                            }
                        }
                        "reverse" => {
                            if a.len() != 1 {
                                return Err("reverse expects list".into());
                            }
                            let mut xs = match &a[0] {
                                Val::List(v) => v.clone(),
                                _ => return Err("reverse expects List".into()),
                            };
                            xs.reverse();
                            Val::List(xs)
                        }
                        "sort" => {
                            if a.len() != 1 {
                                return Err("sort expects list".into());
                            }
                            let mut xs = match &a[0] {
                                Val::List(v) => v.clone(),
                                _ => return Err("sort expects List".into()),
                            };
                            xs.sort_by(|x, y| match (x, y) {
                                (Val::Num(a), Val::Num(b)) => {
                                    a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)
                                }
                                (Val::Str(a), Val::Str(b)) => a.cmp(b),
                                _ => std::cmp::Ordering::Equal,
                            });
                            Val::List(xs)
                        }
                        "unique" => {
                            if a.len() != 1 {
                                return Err("unique expects list".into());
                            }
                            let xs = match &a[0] {
                                Val::List(v) => v,
                                _ => return Err("unique expects List".into()),
                            };
                            let mut out = vec![];
                            for x in xs {
                                if !out.contains(x) {
                                    out.push(x.clone());
                                }
                            }
                            Val::List(out)
                        }
                        "enumerate" => {
                            if a.len() != 1 {
                                return Err("enumerate expects list".into());
                            }
                            let xs = match &a[0] {
                                Val::List(v) => v,
                                _ => return Err("enumerate expects List".into()),
                            };
                            Val::List(
                                xs.iter()
                                    .enumerate()
                                    .map(|(i, v)| Val::List(vec![Val::Num(i as f64), v.clone()]))
                                    .collect(),
                            )
                        }
                        "zip" => {
                            if a.len() != 2 {
                                return Err("zip expects two lists".into());
                            }
                            let x = match &a[0] {
                                Val::List(v) => v,
                                _ => return Err("zip expects Lists".into()),
                            };
                            let y = match &a[1] {
                                Val::List(v) => v,
                                _ => return Err("zip expects Lists".into()),
                            };
                            Val::List(
                                x.iter()
                                    .zip(y.iter())
                                    .map(|(a, b)| Val::List(vec![a.clone(), b.clone()]))
                                    .collect(),
                            )
                        }
                        "map_call" => {
                            if a.len() != 2 {
                                return Err("map_call expects callable,list".into());
                            }
                            let (_, def) = self.callable_def(&a[0])?;
                            let xs = match &a[1] {
                                Val::List(v) => v,
                                _ => return Err("map_call expects List".into()),
                            };
                            let mut out = vec![];
                            for x in xs {
                                out.push(self.call_def(def.clone(), vec![x.clone()])?);
                            }
                            Val::List(out)
                        }
                        "filter_call" => {
                            if a.len() != 2 {
                                return Err("filter_call expects callable,list".into());
                            }
                            let (_, def) = self.callable_def(&a[0])?;
                            let xs = match &a[1] {
                                Val::List(v) => v,
                                _ => return Err("filter_call expects List".into()),
                            };
                            let mut out = vec![];
                            for x in xs {
                                if self.call_def(def.clone(), vec![x.clone()])?.truthy() {
                                    out.push(x.clone());
                                }
                            }
                            Val::List(out)
                        }
                        "reduce_call" => {
                            if a.len() != 3 {
                                return Err("reduce_call expects callable,list,initial".into());
                            }
                            let (_, def) = self.callable_def(&a[0])?;
                            let xs = match &a[1] {
                                Val::List(v) => v,
                                _ => return Err("reduce_call expects List".into()),
                            };
                            let mut acc = a[2].clone();
                            for x in xs {
                                acc = self.call_def(def.clone(), vec![acc, x.clone()])?;
                            }
                            acc
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
                        "regex_match" => {
                            if a.len() != 2 {
                                return Err("regex_match expects pattern,text".into());
                            }
                            let p = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("pattern must be String".into()),
                            };
                            let t = match &a[1] {
                                Val::Str(s) => s,
                                _ => return Err("text must be String".into()),
                            };
                            Val::Bool(Regex::new(p).map_err(|e| e.to_string())?.is_match(t))
                        }
                        "regex_replace" => {
                            if a.len() != 3 {
                                return Err("regex_replace expects pattern,text,replacement".into());
                            }
                            let p = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("pattern must be String".into()),
                            };
                            let t = match &a[1] {
                                Val::Str(s) => s,
                                _ => return Err("text must be String".into()),
                            };
                            let r = match &a[2] {
                                Val::Str(s) => s,
                                _ => return Err("replacement must be String".into()),
                            };
                            Val::Str(
                                Regex::new(p)
                                    .map_err(|e| e.to_string())?
                                    .replace_all(t, r.as_str())
                                    .to_string(),
                            )
                        }
                        "regex_split" => {
                            if a.len() != 2 {
                                return Err("regex_split expects pattern,text".into());
                            }
                            let p = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("pattern must be String".into()),
                            };
                            let t = match &a[1] {
                                Val::Str(s) => s,
                                _ => return Err("text must be String".into()),
                            };
                            Val::List(
                                Regex::new(p)
                                    .map_err(|e| e.to_string())?
                                    .split(t)
                                    .map(|x| Val::Str(x.to_string()))
                                    .collect(),
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
                        "vec_median" => {
                            if a.len() != 1 {
                                return Err("vec_median expects list".into());
                            }
                            let mut xs = as_num_list(&a[0])?;
                            if xs.is_empty() {
                                return Err("vec_median requires non-empty list".into());
                            }
                            xs.sort_by(|x, y| {
                                x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal)
                            });
                            let n = xs.len();
                            Val::Num(if n % 2 == 1 {
                                xs[n / 2]
                            } else {
                                (xs[n / 2 - 1] + xs[n / 2]) / 2.0
                            })
                        }
                        "vec_percentile" => {
                            if a.len() != 2 {
                                return Err("vec_percentile expects list,p".into());
                            }
                            let mut xs = as_num_list(&a[0])?;
                            let p = match a[1] {
                                Val::Num(n) => n.clamp(0.0, 100.0),
                                _ => return Err("p must be Number".into()),
                            };
                            if xs.is_empty() {
                                return Err("vec_percentile requires non-empty list".into());
                            }
                            xs.sort_by(|x, y| {
                                x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal)
                            });
                            let pos = (p / 100.0) * (xs.len() - 1) as f64;
                            let lo = pos.floor() as usize;
                            let hi = pos.ceil() as usize;
                            let value = if lo == hi {
                                xs[lo]
                            } else {
                                xs[lo] + (xs[hi] - xs[lo]) * (pos - lo as f64)
                            };
                            Val::Num(value)
                        }
                        "covariance" => {
                            if a.len() != 2 {
                                return Err("covariance expects two lists".into());
                            }
                            let x = as_num_list(&a[0])?;
                            let y = as_num_list(&a[1])?;
                            if x.len() != y.len() || x.is_empty() {
                                return Err("covariance length mismatch".into());
                            }
                            let mx = x.iter().sum::<f64>() / x.len() as f64;
                            let my = y.iter().sum::<f64>() / y.len() as f64;
                            Val::Num(
                                x.iter()
                                    .zip(y.iter())
                                    .map(|(a, b)| (a - mx) * (b - my))
                                    .sum::<f64>()
                                    / x.len() as f64,
                            )
                        }
                        "correlation" => {
                            if a.len() != 2 {
                                return Err("correlation expects two lists".into());
                            }
                            let x = as_num_list(&a[0])?;
                            let y = as_num_list(&a[1])?;
                            if x.len() != y.len() || x.is_empty() {
                                return Err("correlation length mismatch".into());
                            }
                            let mx = x.iter().sum::<f64>() / x.len() as f64;
                            let my = y.iter().sum::<f64>() / y.len() as f64;
                            let cov = x
                                .iter()
                                .zip(y.iter())
                                .map(|(a, b)| (a - mx) * (b - my))
                                .sum::<f64>();
                            let sx = x.iter().map(|a| (a - mx) * (a - mx)).sum::<f64>().sqrt();
                            let sy = y.iter().map(|b| (b - my) * (b - my)).sum::<f64>().sqrt();
                            Val::Num(if sx == 0.0 || sy == 0.0 {
                                0.0
                            } else {
                                cov / (sx * sy)
                            })
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
                        "matrix_det" => {
                            if a.len() != 1 {
                                return Err("matrix_det expects matrix".into());
                            }
                            Val::Num(matrix_determinant(as_matrix(&a[0])?)?)
                        }
                        "matrix_inverse" => {
                            if a.len() != 1 {
                                return Err("matrix_inverse expects matrix".into());
                            }
                            matrix_to_val(matrix_inverse(as_matrix(&a[0])?)?)
                        }
                        "linear_solve" => {
                            if a.len() != 2 {
                                return Err("linear_solve expects matrix,vector".into());
                            }
                            nums_to_val(solve_linear_system(
                                as_matrix(&a[0])?,
                                as_num_list(&a[1])?,
                            )?)
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
                        "tensor" => {
                            if a.is_empty() || a.len() > 2 {
                                return Err("tensor expects values[,requires_grad]".into());
                            }
                            let requires_grad = if a.len() == 2 {
                                match a[1] {
                                    Val::Bool(b) => b,
                                    _ => return Err("requires_grad must be Bool".into()),
                                }
                            } else {
                                false
                            };
                            tensor_from_value(&a[0], requires_grad)?
                        }
                        "tensor_values" => {
                            if a.len() != 1 {
                                return Err("tensor_values expects tensor".into());
                            }
                            let id = tensor_id(&a[0])?;
                            nums_to_val(tensor_snapshot(id)?.data)
                        }
                        "tensor_shape" => {
                            if a.len() != 1 {
                                return Err("tensor_shape expects tensor".into());
                            }
                            let id = tensor_id(&a[0])?;
                            Val::List(
                                tensor_snapshot(id)?
                                    .shape
                                    .into_iter()
                                    .map(|n| Val::Num(n as f64))
                                    .collect(),
                            )
                        }
                        "tensor_grad" => {
                            if a.len() != 1 {
                                return Err("tensor_grad expects tensor".into());
                            }
                            let id = tensor_id(&a[0])?;
                            nums_to_val(tensor_snapshot(id)?.grad)
                        }
                        "tensor_add" | "tensor_sub" | "tensor_mul" | "tensor_div" => {
                            if a.len() != 2 {
                                return Err(format!("{n} expects two tensors"));
                            }
                            let left_id = tensor_id(&a[0])?;
                            let right_id = tensor_id(&a[1])?;
                            let left = tensor_snapshot(left_id)?;
                            let right = tensor_snapshot(right_id)?;
                            if left.shape != right.shape || left.data.len() != right.data.len() {
                                return Err("tensor shapes must match".into());
                            }
                            if n == "tensor_div" && right.data.contains(&0.0) {
                                return Err("tensor_div division by zero".into());
                            }
                            let data = match n.as_str() {
                                "tensor_add" => left
                                    .data
                                    .iter()
                                    .zip(&right.data)
                                    .map(|(x, y)| x + y)
                                    .collect(),
                                "tensor_sub" => left
                                    .data
                                    .iter()
                                    .zip(&right.data)
                                    .map(|(x, y)| x - y)
                                    .collect(),
                                "tensor_mul" => left
                                    .data
                                    .iter()
                                    .zip(&right.data)
                                    .map(|(x, y)| x * y)
                                    .collect(),
                                _ => left
                                    .data
                                    .iter()
                                    .zip(&right.data)
                                    .map(|(x, y)| x / y)
                                    .collect(),
                            };
                            let requires_grad = left.requires_grad || right.requires_grad;
                            let op = match n.as_str() {
                                "tensor_add" => TensorOp::Add(left_id, right_id),
                                "tensor_sub" => TensorOp::Sub(left_id, right_id),
                                "tensor_mul" => TensorOp::Mul(left_id, right_id),
                                _ => TensorOp::Div(left_id, right_id),
                            };
                            tensor_store(data, left.shape, requires_grad, op)?
                        }
                        "tensor_neg" => {
                            if a.len() != 1 {
                                return Err("tensor_neg expects tensor".into());
                            }
                            let id = tensor_id(&a[0])?;
                            let t = tensor_snapshot(id)?;
                            let data = t.data.iter().map(|x| -*x).collect();
                            tensor_store(data, t.shape, t.requires_grad, TensorOp::Neg(id))?
                        }
                        "tensor_pow" => {
                            if a.len() != 2 {
                                return Err("tensor_pow expects tensor,exponent".into());
                            }
                            let id = tensor_id(&a[0])?;
                            let exponent = match a[1] {
                                Val::Num(n) => n,
                                _ => return Err("tensor_pow exponent must be Number".into()),
                            };
                            let t = tensor_snapshot(id)?;
                            let data = t.data.iter().map(|x| x.powf(exponent)).collect();
                            tensor_store(
                                data,
                                t.shape,
                                t.requires_grad,
                                TensorOp::Pow(id, exponent),
                            )?
                        }
                        "tensor_softmax" => {
                            if a.len() != 1 {
                                return Err("tensor_softmax expects tensor".into());
                            }
                            let id = tensor_id(&a[0])?;
                            let t = tensor_snapshot(id)?;
                            let (rows, cols) = match t.shape.as_slice() {
                                [n] => (1usize, *n),
                                [r, c] => (*r, *c),
                                _ => return Err("tensor_softmax expects 1D or 2D tensor".into()),
                            };
                            let mut out = vec![0.0; t.data.len()];
                            for row in 0..rows {
                                let start = row * cols;
                                let end = start + cols;
                                let max = t.data[start..end]
                                    .iter()
                                    .copied()
                                    .fold(f64::NEG_INFINITY, f64::max);
                                let mut denom = 0.0;
                                for c in 0..cols {
                                    let e = (t.data[start + c] - max).exp();
                                    out[start + c] = e;
                                    denom += e;
                                }
                                for c in 0..cols {
                                    out[start + c] /= denom;
                                }
                            }
                            tensor_store(
                                out,
                                t.shape,
                                t.requires_grad,
                                TensorOp::SoftmaxRows(id, rows, cols),
                            )?
                        }
                        "tensor_detach" => {
                            if a.len() != 1 {
                                return Err("tensor_detach expects tensor".into());
                            }
                            let id = tensor_id(&a[0])?;
                            let t = tensor_snapshot(id)?;
                            tensor_store(t.data, t.shape, false, TensorOp::Leaf)?
                        }
                        "tensor_numel" => {
                            if a.len() != 1 {
                                return Err("tensor_numel expects tensor".into());
                            }
                            let id = tensor_id(&a[0])?;
                            Val::Num(tensor_snapshot(id)?.data.len() as f64)
                        }
                        "tensor_relu" => {
                            if a.len() != 1 {
                                return Err("tensor_relu expects tensor".into());
                            }
                            let id = tensor_id(&a[0])?;
                            let t = tensor_snapshot(id)?;
                            let data = t.data.iter().map(|x| x.max(0.0)).collect();
                            tensor_store(data, t.shape, t.requires_grad, TensorOp::Relu(id))?
                        }
                        "tensor_sigmoid" | "tensor_tanh" | "tensor_exp" | "tensor_log" => {
                            if a.len() != 1 {
                                return Err(format!("{n} expects tensor"));
                            }
                            let id = tensor_id(&a[0])?;
                            let t = tensor_snapshot(id)?;
                            let data: Vec<f64> = match n.as_str() {
                                "tensor_sigmoid" => {
                                    t.data.iter().map(|x| 1.0 / (1.0 + (-x).exp())).collect()
                                }
                                "tensor_tanh" => t.data.iter().map(|x| x.tanh()).collect(),
                                "tensor_exp" => t.data.iter().map(|x| x.exp()).collect(),
                                _ => {
                                    if t.data.iter().any(|x| *x <= 0.0) {
                                        return Err(
                                            "tensor_log requires strictly positive values".into()
                                        );
                                    }
                                    t.data.iter().map(|x| x.ln()).collect()
                                }
                            };
                            let op = match n.as_str() {
                                "tensor_sigmoid" => TensorOp::Sigmoid(id),
                                "tensor_tanh" => TensorOp::Tanh(id),
                                "tensor_exp" => TensorOp::Exp(id),
                                _ => TensorOp::Log(id),
                            };
                            tensor_store(data, t.shape, t.requires_grad, op)?
                        }
                        "tensor_reshape" => {
                            if a.len() != 2 {
                                return Err("tensor_reshape expects tensor,shape".into());
                            }
                            let id = tensor_id(&a[0])?;
                            let t = tensor_snapshot(id)?;
                            let dims = as_num_list(&a[1])?;
                            let mut shape = Vec::with_capacity(dims.len());
                            for d in dims {
                                if d < 0.0 || d.fract() != 0.0 {
                                    return Err(
                                        "tensor_reshape dimensions must be non-negative integers"
                                            .into(),
                                    );
                                }
                                shape.push(d as usize);
                            }
                            let size = if shape.is_empty() {
                                1
                            } else {
                                shape.iter().product()
                            };
                            if size != t.data.len() {
                                return Err("tensor_reshape element count mismatch".into());
                            }
                            tensor_store(t.data, shape, t.requires_grad, TensorOp::Reshape(id))?
                        }
                        "tensor_transpose" => {
                            if a.len() != 1 {
                                return Err("tensor_transpose expects tensor".into());
                            }
                            let id = tensor_id(&a[0])?;
                            let t = tensor_snapshot(id)?;
                            if t.shape.len() != 2 {
                                return Err("tensor_transpose expects a 2D tensor".into());
                            }
                            let (rows, cols) = (t.shape[0], t.shape[1]);
                            let mut out = vec![0.0; t.data.len()];
                            for r in 0..rows {
                                for c in 0..cols {
                                    out[c * rows + r] = t.data[r * cols + c];
                                }
                            }
                            tensor_store(
                                out,
                                vec![cols, rows],
                                t.requires_grad,
                                TensorOp::Transpose(id, rows, cols),
                            )?
                        }
                        "tensor_save" => {
                            if a.len() != 2 {
                                return Err("tensor_save expects tensor,path".into());
                            }
                            let id = tensor_id(&a[0])?;
                            let path = match &a[1] {
                                Val::Str(s) => s,
                                _ => return Err("tensor_save path must be String".into()),
                            };
                            let t = tensor_snapshot(id)?;
                            let body = serde_json::json!({
                                "format": "veyra.tensor.v1",
                                "shape": t.shape,
                                "data": t.data,
                                "requires_grad": t.requires_grad
                            });
                            fs::write(
                                path,
                                serde_json::to_vec_pretty(&body).map_err(|e| e.to_string())?,
                            )
                            .map_err(|e| e.to_string())?;
                            Val::Null
                        }
                        "tensor_load" => {
                            if a.len() != 1 {
                                return Err("tensor_load expects path".into());
                            }
                            let path = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("tensor_load path must be String".into()),
                            };
                            let body: JsonValue =
                                serde_json::from_slice(&fs::read(path).map_err(|e| e.to_string())?)
                                    .map_err(|e| e.to_string())?;
                            if body.get("format").and_then(JsonValue::as_str)
                                != Some("veyra.tensor.v1")
                            {
                                return Err("unsupported tensor file format".into());
                            }
                            let shape = body
                                .get("shape")
                                .and_then(JsonValue::as_array)
                                .ok_or("tensor file missing shape")?
                                .iter()
                                .map(|v| {
                                    v.as_u64().map(|n| n as usize).ok_or("invalid tensor shape")
                                })
                                .collect::<Result<Vec<_>, _>>()?;
                            let data = body
                                .get("data")
                                .and_then(JsonValue::as_array)
                                .ok_or("tensor file missing data")?
                                .iter()
                                .map(|v| v.as_f64().ok_or("invalid tensor value"))
                                .collect::<Result<Vec<_>, _>>()?;
                            let requires_grad = body
                                .get("requires_grad")
                                .and_then(JsonValue::as_bool)
                                .unwrap_or(false);
                            let size = if shape.is_empty() {
                                1
                            } else {
                                shape.iter().product()
                            };
                            if size != data.len() {
                                return Err("tensor file shape/data mismatch".into());
                            }
                            tensor_store(data, shape, requires_grad, TensorOp::Leaf)?
                        }
                        "tensor_sum" | "tensor_mean" => {
                            if a.len() != 1 {
                                return Err(format!("{n} expects tensor"));
                            }
                            let id = tensor_id(&a[0])?;
                            let t = tensor_snapshot(id)?;
                            let sum: f64 = t.data.iter().sum();
                            let (value, op) = if n == "tensor_mean" {
                                (
                                    if t.data.is_empty() {
                                        0.0
                                    } else {
                                        sum / t.data.len() as f64
                                    },
                                    TensorOp::Mean(id),
                                )
                            } else {
                                (sum, TensorOp::Sum(id))
                            };
                            tensor_store(vec![value], vec![], t.requires_grad, op)?
                        }
                        "tensor_matmul" => {
                            if a.len() != 2 {
                                return Err("tensor_matmul expects two tensors".into());
                            }
                            let left_id = tensor_id(&a[0])?;
                            let right_id = tensor_id(&a[1])?;
                            let left = tensor_snapshot(left_id)?;
                            let right = tensor_snapshot(right_id)?;
                            if left.shape.len() != 2 || right.shape.len() != 2 {
                                return Err("tensor_matmul expects 2D tensors".into());
                            }
                            let (m, k) = (left.shape[0], left.shape[1]);
                            let (k2, ncols) = (right.shape[0], right.shape[1]);
                            if k != k2 {
                                return Err("tensor_matmul shape mismatch".into());
                            }
                            let mut out = vec![0.0; m * ncols];
                            for i in 0..m {
                                for j in 0..ncols {
                                    let mut s = 0.0;
                                    for p in 0..k {
                                        s += left.data[i * k + p] * right.data[p * ncols + j];
                                    }
                                    out[i * ncols + j] = s;
                                }
                            }
                            tensor_store(
                                out,
                                vec![m, ncols],
                                left.requires_grad || right.requires_grad,
                                TensorOp::MatMul(left_id, right_id, m, k, ncols),
                            )?
                        }
                        "tensor_backward" => {
                            if a.len() != 1 {
                                return Err("tensor_backward expects scalar tensor".into());
                            }
                            let id = tensor_id(&a[0])?;
                            tensor_backward(id)?;
                            Val::Null
                        }
                        "tensor_step" => {
                            if a.len() != 2 {
                                return Err("tensor_step expects tensor,learning_rate".into());
                            }
                            let id = tensor_id(&a[0])?;
                            let lr = match a[1] {
                                Val::Num(n) => n,
                                _ => return Err("learning_rate must be Number".into()),
                            };
                            let mut store =
                                tensors().lock().map_err(|_| "tensor registry poisoned")?;
                            let t = store.get_mut(&id).ok_or("unknown tensor")?;
                            if !t.requires_grad {
                                return Err("tensor_step requires trainable tensor".into());
                            }
                            for i in 0..t.data.len() {
                                t.data[i] -= lr * t.grad[i];
                            }
                            Val::Null
                        }
                        "tensor_adam_step" => {
                            if a.len() != 5 {
                                return Err(
                                    "tensor_adam_step expects tensor,lr,beta1,beta2,epsilon".into(),
                                );
                            }
                            let id = tensor_id(&a[0])?;
                            let number = |v: &Val, name: &str| match v {
                                Val::Num(n) => Ok(*n),
                                _ => Err(format!("{name} must be Number")),
                            };
                            let lr = number(&a[1], "learning_rate")?;
                            let beta1 = number(&a[2], "beta1")?;
                            let beta2 = number(&a[3], "beta2")?;
                            let epsilon = number(&a[4], "epsilon")?;
                            if !(0.0..1.0).contains(&beta1)
                                || !(0.0..1.0).contains(&beta2)
                                || lr <= 0.0
                                || epsilon <= 0.0
                            {
                                return Err("invalid Adam hyperparameters".into());
                            }
                            let tensor = tensor_snapshot(id)?;
                            if !tensor.requires_grad {
                                return Err("tensor_adam_step requires trainable tensor".into());
                            }
                            let updates = {
                                let mut states =
                                    adam_states().lock().map_err(|_| "adam state poisoned")?;
                                let state = states.entry(id).or_insert_with(|| AdamState {
                                    m: vec![0.0; tensor.data.len()],
                                    v: vec![0.0; tensor.data.len()],
                                    step: 0,
                                });
                                if state.m.len() != tensor.data.len() {
                                    *state = AdamState {
                                        m: vec![0.0; tensor.data.len()],
                                        v: vec![0.0; tensor.data.len()],
                                        step: 0,
                                    };
                                }
                                state.step += 1;
                                let bias1 = 1.0 - beta1.powf(state.step as f64);
                                let bias2 = 1.0 - beta2.powf(state.step as f64);
                                let mut updates = vec![0.0; tensor.data.len()];
                                for i in 0..tensor.data.len() {
                                    let g = tensor.grad[i];
                                    state.m[i] = beta1 * state.m[i] + (1.0 - beta1) * g;
                                    state.v[i] = beta2 * state.v[i] + (1.0 - beta2) * g * g;
                                    let m_hat = state.m[i] / bias1;
                                    let v_hat = state.v[i] / bias2;
                                    updates[i] = lr * m_hat / (v_hat.sqrt() + epsilon);
                                }
                                updates
                            };
                            let mut store =
                                tensors().lock().map_err(|_| "tensor registry poisoned")?;
                            let t = store.get_mut(&id).ok_or("unknown tensor")?;
                            for (value, update) in t.data.iter_mut().zip(updates) {
                                *value -= update;
                            }
                            Val::Null
                        }
                        "tensor_reset_optimizer" => {
                            if a.len() != 1 {
                                return Err("tensor_reset_optimizer expects tensor".into());
                            }
                            let id = tensor_id(&a[0])?;
                            adam_states()
                                .lock()
                                .map_err(|_| "adam state poisoned")?
                                .remove(&id);
                            Val::Null
                        }
                        "tensor_zero_grad" => {
                            if a.len() != 1 {
                                return Err("tensor_zero_grad expects tensor".into());
                            }
                            let id = tensor_id(&a[0])?;
                            let mut store =
                                tensors().lock().map_err(|_| "tensor registry poisoned")?;
                            let t = store.get_mut(&id).ok_or("unknown tensor")?;
                            for g in &mut t.grad {
                                *g = 0.0;
                            }
                            Val::Null
                        }
                        "accuracy_score" => {
                            if a.len() != 2 {
                                return Err("accuracy_score expects actual,predicted".into());
                            }
                            let actual = match &a[0] {
                                Val::List(v) => v,
                                _ => return Err("actual must be List".into()),
                            };
                            let predicted = match &a[1] {
                                Val::List(v) => v,
                                _ => return Err("predicted must be List".into()),
                            };
                            if actual.len() != predicted.len() || actual.is_empty() {
                                return Err("accuracy_score length mismatch".into());
                            }
                            let correct = actual
                                .iter()
                                .zip(predicted.iter())
                                .filter(|(x, y)| x == y)
                                .count();
                            Val::Num(correct as f64 / actual.len() as f64)
                        }
                        "classification_metrics" => {
                            if a.len() != 2 {
                                return Err(
                                    "classification_metrics expects actual,predicted".into()
                                );
                            }
                            let actual = match &a[0] {
                                Val::List(v) => v,
                                _ => return Err("actual must be List".into()),
                            };
                            let predicted = match &a[1] {
                                Val::List(v) => v,
                                _ => return Err("predicted must be List".into()),
                            };
                            if actual.len() != predicted.len() || actual.is_empty() {
                                return Err("classification_metrics length mismatch".into());
                            }
                            let positive = |v: &Val| {
                                matches!(v,Val::Num(n) if *n==1.0) || matches!(v, Val::Bool(true))
                            };
                            let mut tp = 0.0;
                            let mut tn = 0.0;
                            let mut fp = 0.0;
                            let mut fnn = 0.0;
                            for (x, y) in actual.iter().zip(predicted.iter()) {
                                match (positive(x), positive(y)) {
                                    (true, true) => tp += 1.0,
                                    (false, false) => tn += 1.0,
                                    (false, true) => fp += 1.0,
                                    (true, false) => fnn += 1.0,
                                }
                            }
                            let precision = if tp + fp == 0.0 { 0.0 } else { tp / (tp + fp) };
                            let recall = if tp + fnn == 0.0 {
                                0.0
                            } else {
                                tp / (tp + fnn)
                            };
                            let f1 = if precision + recall == 0.0 {
                                0.0
                            } else {
                                2.0 * precision * recall / (precision + recall)
                            };
                            let accuracy = (tp + tn) / (tp + tn + fp + fnn);
                            Val::Map(vec![
                                (Val::Str("accuracy".into()), Val::Num(accuracy)),
                                (Val::Str("precision".into()), Val::Num(precision)),
                                (Val::Str("recall".into()), Val::Num(recall)),
                                (Val::Str("f1".into()), Val::Num(f1)),
                                (Val::Str("tp".into()), Val::Num(tp)),
                                (Val::Str("tn".into()), Val::Num(tn)),
                                (Val::Str("fp".into()), Val::Num(fp)),
                                (Val::Str("fn".into()), Val::Num(fnn)),
                            ])
                        }
                        "train_test_split" => {
                            if a.len() != 3 {
                                return Err("train_test_split expects x,y,test_ratio".into());
                            }
                            let x = match &a[0] {
                                Val::List(v) => v,
                                _ => return Err("x must be List".into()),
                            };
                            let y = match &a[1] {
                                Val::List(v) => v,
                                _ => return Err("y must be List".into()),
                            };
                            let ratio = match a[2] {
                                Val::Num(n) => n,
                                _ => return Err("test_ratio must be Number".into()),
                            };
                            if x.len() != y.len() || x.is_empty() {
                                return Err("train_test_split length mismatch".into());
                            }
                            if !(0.0..1.0).contains(&ratio) {
                                return Err("test_ratio must be between 0 and 1".into());
                            }
                            let test_count = ((x.len() as f64) * ratio).round().max(1.0) as usize;
                            let split = x.len().saturating_sub(test_count.min(x.len()));
                            Val::Map(vec![
                                (Val::Str("train_x".into()), Val::List(x[..split].to_vec())),
                                (Val::Str("train_y".into()), Val::List(y[..split].to_vec())),
                                (Val::Str("test_x".into()), Val::List(x[split..].to_vec())),
                                (Val::Str("test_y".into()), Val::List(y[split..].to_vec())),
                            ])
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
                        "scatter_svg" => {
                            if a.len() != 3 {
                                return Err("scatter_svg expects path,xs,ys".into());
                            }
                            let path = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("path must be String".into()),
                            };
                            let xs = as_num_list(&a[1])?;
                            let ys = as_num_list(&a[2])?;
                            if xs.len() != ys.len() || xs.is_empty() {
                                return Err("scatter_svg requires equal non-empty lists".into());
                            }
                            let xmin = xs.iter().fold(f64::INFINITY, |m, x| m.min(*x));
                            let xmax = xs.iter().fold(f64::NEG_INFINITY, |m, x| m.max(*x));
                            let ymin = ys.iter().fold(f64::INFINITY, |m, x| m.min(*x));
                            let ymax = ys.iter().fold(f64::NEG_INFINITY, |m, x| m.max(*x));
                            let sx = if xmax == xmin { 1.0 } else { xmax - xmin };
                            let sy = if ymax == ymin { 1.0 } else { ymax - ymin };
                            let dots = xs
                                .iter()
                                .zip(ys.iter())
                                .map(|(x, y)| {
                                    let px = 50.0 + 700.0 * (x - xmin) / sx;
                                    let py = 450.0 - 400.0 * (y - ymin) / sy;
                                    format!("<circle cx=\"{px:.2}\" cy=\"{py:.2}\" r=\"4\"/>")
                                })
                                .collect::<Vec<_>>()
                                .join("");
                            let svg = format!(
                                "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"800\" height=\"500\"><rect width=\"100%\" height=\"100%\" fill=\"white\"/><line x1=\"50\" y1=\"450\" x2=\"750\" y2=\"450\" stroke=\"black\"/><line x1=\"50\" y1=\"50\" x2=\"50\" y2=\"450\" stroke=\"black\"/>{dots}</svg>"
                            );
                            fs::write(path, svg).map_err(|e| e.to_string())?;
                            Val::Null
                        }
                        "bar_svg" => {
                            if a.len() != 3 {
                                return Err("bar_svg expects path,labels,values".into());
                            }
                            let path = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("path must be String".into()),
                            };
                            let labels = match &a[1] {
                                Val::List(v) => v,
                                _ => return Err("labels must be List".into()),
                            };
                            let vals = as_num_list(&a[2])?;
                            if labels.len() != vals.len() || vals.is_empty() {
                                return Err("bar_svg requires equal non-empty lists".into());
                            }
                            let max = vals.iter().copied().fold(0.0, f64::max).max(1.0);
                            let width = 700.0 / vals.len() as f64;
                            let mut body = String::new();
                            for (i, (label, value)) in labels.iter().zip(vals.iter()).enumerate() {
                                let h = 360.0 * (*value) / max;
                                let x = 55.0 + i as f64 * width;
                                let y = 440.0 - h;
                                body.push_str(&format!("<rect x=\"{x:.2}\" y=\"{y:.2}\" width=\"{:.2}\" height=\"{h:.2}\"/><text x=\"{:.2}\" y=\"470\" font-size=\"12\" text-anchor=\"middle\">{}</text>",(width-10.0).max(1.0),x+(width-10.0).max(1.0)/2.0,svg_escape(&label.show())));
                            }
                            let svg = format!(
                                "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"800\" height=\"500\"><rect width=\"100%\" height=\"100%\" fill=\"white\"/><line x1=\"50\" y1=\"440\" x2=\"760\" y2=\"440\" stroke=\"black\"/>{body}</svg>"
                            );
                            fs::write(path, svg).map_err(|e| e.to_string())?;
                            Val::Null
                        }
                        "histogram" => {
                            if a.len() != 2 {
                                return Err("histogram expects values,bins".into());
                            }
                            let vals = as_num_list(&a[0])?;
                            let bins = match a[1] {
                                Val::Num(n) => n.max(1.0) as usize,
                                _ => return Err("bins must be Number".into()),
                            };
                            if vals.is_empty() {
                                return Err("histogram requires values".into());
                            }
                            let min = vals.iter().copied().fold(f64::INFINITY, f64::min);
                            let max = vals.iter().copied().fold(f64::NEG_INFINITY, f64::max);
                            let span = (max - min).max(f64::EPSILON);
                            let mut counts = vec![0usize; bins];
                            for v in vals {
                                let mut idx = (((v - min) / span) * bins as f64).floor() as usize;
                                if idx >= bins {
                                    idx = bins - 1;
                                }
                                counts[idx] += 1;
                            }
                            Val::List(
                                counts
                                    .into_iter()
                                    .enumerate()
                                    .map(|(i, count)| {
                                        let lo = min + span * i as f64 / bins as f64;
                                        let hi = min + span * (i + 1) as f64 / bins as f64;
                                        Val::Map(vec![
                                            (Val::Str("low".into()), Val::Num(lo)),
                                            (Val::Str("high".into()), Val::Num(hi)),
                                            (Val::Str("count".into()), Val::Num(count as f64)),
                                        ])
                                    })
                                    .collect(),
                            )
                        }
                        "logreg_fit" => {
                            if a.len() != 4 {
                                return Err(
                                    "logreg_fit expects X,y,iterations,learning_rate".into()
                                );
                            }
                            let x = as_matrix(&a[0])?;
                            let y = as_num_list(&a[1])?;
                            let iterations = match a[2] {
                                Val::Num(n) => n.max(1.0) as usize,
                                _ => return Err("iterations must be Number".into()),
                            };
                            let lr = match a[3] {
                                Val::Num(n) => n,
                                _ => return Err("learning_rate must be Number".into()),
                            };
                            if x.is_empty() || x.len() != y.len() {
                                return Err("logreg_fit row mismatch".into());
                            }
                            let dims = x[0].len();
                            if dims == 0 || x.iter().any(|r| r.len() != dims) {
                                return Err("invalid feature matrix".into());
                            }
                            let mut w = vec![0.0; dims];
                            let mut b = 0.0;
                            for _ in 0..iterations {
                                let mut gw = vec![0.0; dims];
                                let mut gb = 0.0;
                                for (row, target) in x.iter().zip(y.iter()) {
                                    let z = b + w
                                        .iter()
                                        .zip(row.iter())
                                        .map(|(wi, xi)| wi * xi)
                                        .sum::<f64>();
                                    let p = 1.0 / (1.0 + (-z).exp());
                                    let e = p - target;
                                    for j in 0..dims {
                                        gw[j] += e * row[j];
                                    }
                                    gb += e;
                                }
                                let n = x.len() as f64;
                                for j in 0..dims {
                                    w[j] -= lr * gw[j] / n;
                                }
                                b -= lr * gb / n;
                            }
                            Val::Map(vec![
                                (Val::Str("weights".into()), nums_to_val(w)),
                                (Val::Str("bias".into()), Val::Num(b)),
                            ])
                        }
                        "logreg_predict" => {
                            if a.len() != 2 {
                                return Err("logreg_predict expects model,features".into());
                            }
                            let model = match &a[0] {
                                Val::Map(p) => p,
                                _ => return Err("model must be Map".into()),
                            };
                            let features = as_num_list(&a[1])?;
                            let weights = map_lookup(model, &Val::Str("weights".into()))
                                .ok_or("model missing weights")?;
                            let weights = as_num_list(weights)?;
                            let bias = map_lookup(model, &Val::Str("bias".into()))
                                .and_then(|v| if let Val::Num(n) = v { Some(*n) } else { None })
                                .ok_or("model missing bias")?;
                            if weights.len() != features.len() {
                                return Err("logreg_predict dimension mismatch".into());
                            }
                            let z = bias
                                + weights
                                    .iter()
                                    .zip(features.iter())
                                    .map(|(w, x)| w * x)
                                    .sum::<f64>();
                            Val::Num(1.0 / (1.0 + (-z).exp()))
                        }
                        "table_group_count" => {
                            if a.len() != 2 {
                                return Err("table_group_count expects rows,name".into());
                            }
                            let rows = match &a[0] {
                                Val::List(v) => v,
                                _ => return Err("rows must be List".into()),
                            };
                            let key = match &a[1] {
                                Val::Str(s) => Val::Str(s.clone()),
                                _ => return Err("name must be String".into()),
                            };
                            let mut groups: Vec<(Val, usize)> = vec![];
                            for row in rows {
                                let pairs = match row {
                                    Val::Map(p) => p,
                                    _ => return Err("rows must contain Maps".into()),
                                };
                                let value = map_lookup(pairs, &key).cloned().unwrap_or(Val::Null);
                                if let Some((_, n)) = groups.iter_mut().find(|(v, _)| *v == value) {
                                    *n += 1
                                } else {
                                    groups.push((value, 1))
                                }
                            }
                            Val::List(
                                groups
                                    .into_iter()
                                    .map(|(value, count)| {
                                        Val::Map(vec![
                                            (Val::Str("value".into()), value),
                                            (Val::Str("count".into()), Val::Num(count as f64)),
                                        ])
                                    })
                                    .collect(),
                            )
                        }
                        "table_group_sum" => {
                            if a.len() != 3 {
                                return Err("table_group_sum expects rows,group,value".into());
                            }
                            let rows = match &a[0] {
                                Val::List(v) => v,
                                _ => return Err("rows must be List".into()),
                            };
                            let gk = match &a[1] {
                                Val::Str(s) => Val::Str(s.clone()),
                                _ => return Err("group must be String".into()),
                            };
                            let vk = match &a[2] {
                                Val::Str(s) => Val::Str(s.clone()),
                                _ => return Err("value must be String".into()),
                            };
                            let mut groups: Vec<(Val, f64)> = vec![];
                            for row in rows {
                                let p = match row {
                                    Val::Map(p) => p,
                                    _ => return Err("rows must contain Maps".into()),
                                };
                                let group = map_lookup(p, &gk).cloned().unwrap_or(Val::Null);
                                let value = map_lookup(p, &vk)
                                    .and_then(|v| if let Val::Num(n) = v { Some(*n) } else { None })
                                    .ok_or("table_group_sum requires numeric value column")?;
                                if let Some((_, sum)) = groups.iter_mut().find(|(g, _)| *g == group)
                                {
                                    *sum += value
                                } else {
                                    groups.push((group, value));
                                }
                            }
                            Val::List(
                                groups
                                    .into_iter()
                                    .map(|(group, sum)| {
                                        Val::Map(vec![
                                            (Val::Str("value".into()), group),
                                            (Val::Str("sum".into()), Val::Num(sum)),
                                        ])
                                    })
                                    .collect(),
                            )
                        }
                        "table_group_mean" => {
                            if a.len() != 3 {
                                return Err("table_group_mean expects rows,group,value".into());
                            }
                            let rows = match &a[0] {
                                Val::List(v) => v,
                                _ => return Err("rows must be List".into()),
                            };
                            let gk = match &a[1] {
                                Val::Str(s) => Val::Str(s.clone()),
                                _ => return Err("group must be String".into()),
                            };
                            let vk = match &a[2] {
                                Val::Str(s) => Val::Str(s.clone()),
                                _ => return Err("value must be String".into()),
                            };
                            let mut groups: Vec<(Val, f64, usize)> = vec![];
                            for row in rows {
                                let p = match row {
                                    Val::Map(p) => p,
                                    _ => return Err("rows must contain Maps".into()),
                                };
                                let group = map_lookup(p, &gk).cloned().unwrap_or(Val::Null);
                                let value = map_lookup(p, &vk)
                                    .and_then(|v| if let Val::Num(n) = v { Some(*n) } else { None })
                                    .ok_or("table_group_mean requires numeric value column")?;
                                if let Some((_, sum, count)) =
                                    groups.iter_mut().find(|(g, _, _)| *g == group)
                                {
                                    *sum += value;
                                    *count += 1
                                } else {
                                    groups.push((group, value, 1));
                                }
                            }
                            Val::List(
                                groups
                                    .into_iter()
                                    .map(|(group, sum, count)| {
                                        Val::Map(vec![
                                            (Val::Str("value".into()), group),
                                            (Val::Str("mean".into()), Val::Num(sum / count as f64)),
                                            (Val::Str("count".into()), Val::Num(count as f64)),
                                        ])
                                    })
                                    .collect(),
                            )
                        }
                        "table_sort" => {
                            if a.len() != 2 {
                                return Err("table_sort expects rows,name".into());
                            }
                            let mut rows = match &a[0] {
                                Val::List(v) => v.clone(),
                                _ => return Err("rows must be List".into()),
                            };
                            let key = match &a[1] {
                                Val::Str(s) => Val::Str(s.clone()),
                                _ => return Err("name must be String".into()),
                            };
                            rows.sort_by(|ra, rb| {
                                let av = if let Val::Map(p) = ra {
                                    map_lookup(p, &key)
                                } else {
                                    None
                                };
                                let bv = if let Val::Map(p) = rb {
                                    map_lookup(p, &key)
                                } else {
                                    None
                                };
                                match (av, bv) {
                                    (Some(Val::Num(a)), Some(Val::Num(b))) => {
                                        a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)
                                    }
                                    (Some(Val::Str(a)), Some(Val::Str(b))) => a.cmp(b),
                                    _ => std::cmp::Ordering::Equal,
                                }
                            });
                            Val::List(rows)
                        }
                        "table_select" => {
                            if a.len() != 2 {
                                return Err("table_select expects rows,names".into());
                            }
                            let rows = match &a[0] {
                                Val::List(v) => v,
                                _ => return Err("rows must be List".into()),
                            };
                            let names = match &a[1] {
                                Val::List(v) => v,
                                _ => return Err("names must be List".into()),
                            };
                            let keys: Result<Vec<String>, String> = names
                                .iter()
                                .map(|v| match v {
                                    Val::Str(s) => Ok(s.clone()),
                                    _ => Err("column names must be Strings".into()),
                                })
                                .collect();
                            let keys = keys?;
                            let mut out = vec![];
                            for row in rows {
                                let pairs = match row {
                                    Val::Map(p) => p,
                                    _ => return Err("rows must contain Maps".into()),
                                };
                                let mut selected = vec![];
                                for name in &keys {
                                    let k = Val::Str(name.clone());
                                    selected.push((
                                        k.clone(),
                                        map_lookup(pairs, &k).cloned().unwrap_or(Val::Null),
                                    ));
                                }
                                out.push(Val::Map(selected));
                            }
                            Val::List(out)
                        }
                        "table_inner_join" => {
                            if a.len() != 4 {
                                return Err(
                                    "table_inner_join expects left,right,left_key,right_key".into(),
                                );
                            }
                            let left = match &a[0] {
                                Val::List(v) => v,
                                _ => return Err("left must be List".into()),
                            };
                            let right = match &a[1] {
                                Val::List(v) => v,
                                _ => return Err("right must be List".into()),
                            };
                            let lk = match &a[2] {
                                Val::Str(s) => Val::Str(s.clone()),
                                _ => return Err("left_key must be String".into()),
                            };
                            let rk = match &a[3] {
                                Val::Str(s) => Val::Str(s.clone()),
                                _ => return Err("right_key must be String".into()),
                            };
                            let mut out = vec![];
                            for lrow in left {
                                let lp = match lrow {
                                    Val::Map(p) => p,
                                    _ => return Err("left rows must be Maps".into()),
                                };
                                let lv = map_lookup(lp, &lk);
                                for rrow in right {
                                    let rp = match rrow {
                                        Val::Map(p) => p,
                                        _ => return Err("right rows must be Maps".into()),
                                    };
                                    if lv.is_some() && lv == map_lookup(rp, &rk) {
                                        let mut merged = lp.clone();
                                        for (k, v) in rp {
                                            let key =
                                                if merged.iter().any(|(existing, _)| existing == k)
                                                {
                                                    Val::Str(format!("right.{}", k.show()))
                                                } else {
                                                    k.clone()
                                                };
                                            merged.push((key, v.clone()));
                                        }
                                        out.push(Val::Map(merged));
                                    }
                                }
                            }
                            Val::List(out)
                        }
                        "table_head" => {
                            if a.len() != 2 {
                                return Err("table_head expects rows,count".into());
                            }
                            let rows = match &a[0] {
                                Val::List(v) => v,
                                _ => return Err("rows must be List".into()),
                            };
                            let n = match a[1] {
                                Val::Num(n) => n.max(0.0) as usize,
                                _ => return Err("count must be Number".into()),
                            };
                            Val::List(rows.iter().take(n).cloned().collect())
                        }
                        "table_tail" => {
                            if a.len() != 2 {
                                return Err("table_tail expects rows,count".into());
                            }
                            let rows = match &a[0] {
                                Val::List(v) => v,
                                _ => return Err("rows must be List".into()),
                            };
                            let n = match a[1] {
                                Val::Num(n) => n.max(0.0) as usize,
                                _ => return Err("count must be Number".into()),
                            };
                            let start = rows.len().saturating_sub(n);
                            Val::List(rows[start..].to_vec())
                        }
                        "table_drop_nulls" => {
                            if a.len() != 2 {
                                return Err("table_drop_nulls expects rows,column".into());
                            }
                            let rows = match &a[0] {
                                Val::List(v) => v,
                                _ => return Err("rows must be List".into()),
                            };
                            let key = match &a[1] {
                                Val::Str(s) => Val::Str(s.clone()),
                                _ => return Err("column must be String".into()),
                            };
                            Val::List(
                                rows.iter()
                                    .filter(|row| match row {
                                        Val::Map(p) => map_lookup(p, &key)
                                            .map(|v| !matches!(v, Val::Null))
                                            .unwrap_or(false),
                                        _ => false,
                                    })
                                    .cloned()
                                    .collect(),
                            )
                        }
                        "table_fill_null" => {
                            if a.len() != 3 {
                                return Err("table_fill_null expects rows,column,value".into());
                            }
                            let rows = match &a[0] {
                                Val::List(v) => v,
                                _ => return Err("rows must be List".into()),
                            };
                            let key = match &a[1] {
                                Val::Str(s) => Val::Str(s.clone()),
                                _ => return Err("column must be String".into()),
                            };
                            let fill = a[2].clone();
                            let mut out = vec![];
                            for row in rows {
                                let mut pairs = match row {
                                    Val::Map(p) => p.clone(),
                                    _ => return Err("rows must contain Maps".into()),
                                };
                                if let Some((_, v)) = pairs.iter_mut().find(|(k, _)| *k == key) {
                                    if matches!(v, Val::Null) {
                                        *v = fill.clone();
                                    }
                                } else {
                                    pairs.push((key.clone(), fill.clone()));
                                }
                                out.push(Val::Map(pairs));
                            }
                            Val::List(out)
                        }
                        "table_left_join" => {
                            if a.len() != 4 {
                                return Err(
                                    "table_left_join expects left,right,left_key,right_key".into(),
                                );
                            }
                            let left = match &a[0] {
                                Val::List(v) => v,
                                _ => return Err("left must be List".into()),
                            };
                            let right = match &a[1] {
                                Val::List(v) => v,
                                _ => return Err("right must be List".into()),
                            };
                            let lk = match &a[2] {
                                Val::Str(s) => Val::Str(s.clone()),
                                _ => return Err("left_key must be String".into()),
                            };
                            let rk = match &a[3] {
                                Val::Str(s) => Val::Str(s.clone()),
                                _ => return Err("right_key must be String".into()),
                            };
                            let mut out = vec![];
                            for lrow in left {
                                let lp = match lrow {
                                    Val::Map(p) => p,
                                    _ => return Err("left rows must be Maps".into()),
                                };
                                let lv = map_lookup(lp, &lk);
                                let mut matched = false;
                                for rrow in right {
                                    let rp = match rrow {
                                        Val::Map(p) => p,
                                        _ => return Err("right rows must be Maps".into()),
                                    };
                                    if lv.is_some() && lv == map_lookup(rp, &rk) {
                                        matched = true;
                                        let mut merged = lp.clone();
                                        for (k, v) in rp {
                                            let key =
                                                if merged.iter().any(|(existing, _)| existing == k)
                                                {
                                                    Val::Str(format!("right.{}", k.show()))
                                                } else {
                                                    k.clone()
                                                };
                                            merged.push((key, v.clone()));
                                        }
                                        out.push(Val::Map(merged));
                                    }
                                }
                                if !matched {
                                    out.push(Val::Map(lp.clone()));
                                }
                            }
                            Val::List(out)
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
                        #[cfg(feature = "native-services")]
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
                        #[cfg(feature = "native-services")]
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
                        #[cfg(feature = "native-services")]
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
                        #[cfg(feature = "native-services")]
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
                        #[cfg(feature = "native-services")]
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
                        #[cfg(feature = "native-services")]
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
                        #[cfg(feature = "native-services")]
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
                        #[cfg(feature = "native-services")]
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
                        #[cfg(feature = "native-services")]
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
                        "sin" => {
                            if a.len() != 1 {
                                return Err("sin expects number".into());
                            }
                            match a[0] {
                                Val::Num(n) => Val::Num(n.sin()),
                                _ => return Err("sin expects Number".into()),
                            }
                        }
                        "cos" => {
                            if a.len() != 1 {
                                return Err("cos expects number".into());
                            }
                            match a[0] {
                                Val::Num(n) => Val::Num(n.cos()),
                                _ => return Err("cos expects Number".into()),
                            }
                        }
                        "tan" => {
                            if a.len() != 1 {
                                return Err("tan expects number".into());
                            }
                            match a[0] {
                                Val::Num(n) => Val::Num(n.tan()),
                                _ => return Err("tan expects Number".into()),
                            }
                        }
                        "exp" => {
                            if a.len() != 1 {
                                return Err("exp expects number".into());
                            }
                            match a[0] {
                                Val::Num(n) => Val::Num(n.exp()),
                                _ => return Err("exp expects Number".into()),
                            }
                        }
                        "ln" => {
                            if a.len() != 1 {
                                return Err("ln expects number".into());
                            }
                            match a[0] {
                                Val::Num(n) => Val::Num(n.ln()),
                                _ => return Err("ln expects Number".into()),
                            }
                        }
                        "log10" => {
                            if a.len() != 1 {
                                return Err("log10 expects number".into());
                            }
                            match a[0] {
                                Val::Num(n) => Val::Num(n.log10()),
                                _ => return Err("log10 expects Number".into()),
                            }
                        }
                        "pow" => {
                            if a.len() != 2 {
                                return Err("pow expects base,exponent".into());
                            }
                            match (a[0].clone(), a[1].clone()) {
                                (Val::Num(x), Val::Num(y)) => Val::Num(x.powf(y)),
                                _ => return Err("pow expects Numbers".into()),
                            }
                        }
                        "round" => {
                            if a.len() != 1 {
                                return Err("round expects number".into());
                            }
                            match a[0] {
                                Val::Num(n) => Val::Num(n.round()),
                                _ => return Err("round expects Number".into()),
                            }
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
                        "starts_with" => {
                            if a.len() != 2 {
                                return Err("starts_with expects text,prefix".into());
                            }
                            let text = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("text must be String".into()),
                            };
                            let prefix = match &a[1] {
                                Val::Str(s) => s,
                                _ => return Err("prefix must be String".into()),
                            };
                            Val::Bool(text.starts_with(prefix))
                        }
                        "ends_with" => {
                            if a.len() != 2 {
                                return Err("ends_with expects text,suffix".into());
                            }
                            let text = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("text must be String".into()),
                            };
                            let suffix = match &a[1] {
                                Val::Str(s) => s,
                                _ => return Err("suffix must be String".into()),
                            };
                            Val::Bool(text.ends_with(suffix))
                        }
                        "replace" => {
                            if a.len() != 3 {
                                return Err("replace expects text,from,to".into());
                            }
                            let text = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("text must be String".into()),
                            };
                            let from = match &a[1] {
                                Val::Str(s) => s,
                                _ => return Err("from must be String".into()),
                            };
                            let to = match &a[2] {
                                Val::Str(s) => s,
                                _ => return Err("to must be String".into()),
                            };
                            Val::Str(text.replace(from, to))
                        }
                        "repeat" => {
                            if a.len() != 2 {
                                return Err("repeat expects text,count".into());
                            }
                            let text = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("text must be String".into()),
                            };
                            let count = match a[1] {
                                Val::Num(n) => n.max(0.0) as usize,
                                _ => return Err("count must be Number".into()),
                            };
                            Val::Str(text.repeat(count))
                        }
                        "chars" => {
                            if a.len() != 1 {
                                return Err("chars expects text".into());
                            }
                            let text = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("text must be String".into()),
                            };
                            Val::List(text.chars().map(|c| Val::Str(c.to_string())).collect())
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
                        "replace_text" => {
                            if a.len() != 3 {
                                return Err("replace_text expects text,from,to".into());
                            }
                            let text = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("text must be String".into()),
                            };
                            let from = match &a[1] {
                                Val::Str(s) => s,
                                _ => return Err("from must be String".into()),
                            };
                            let to = match &a[2] {
                                Val::Str(s) => s,
                                _ => return Err("to must be String".into()),
                            };
                            Val::Str(text.replace(from, to))
                        }
                        "path_join" => {
                            if a.is_empty() {
                                return Err("path_join expects at least one part".into());
                            }
                            let mut p = PathBuf::new();
                            for part in &a {
                                match part {
                                    Val::Str(s) => p.push(s),
                                    _ => return Err("path parts must be Strings".into()),
                                }
                            }
                            Val::Str(p.display().to_string())
                        }
                        "path_name" => {
                            if a.len() != 1 {
                                return Err("path_name expects path".into());
                            }
                            let p = match &a[0] {
                                Val::Str(s) => Path::new(s),
                                _ => return Err("path must be String".into()),
                            };
                            Val::Str(
                                p.file_name()
                                    .map(|x| x.to_string_lossy().to_string())
                                    .unwrap_or_default(),
                            )
                        }
                        "path_parent" => {
                            if a.len() != 1 {
                                return Err("path_parent expects path".into());
                            }
                            let p = match &a[0] {
                                Val::Str(s) => Path::new(s),
                                _ => return Err("path must be String".into()),
                            };
                            p.parent()
                                .map(|x| Val::Str(x.display().to_string()))
                                .unwrap_or(Val::Null)
                        }
                        "path_extension" => {
                            if a.len() != 1 {
                                return Err("path_extension expects path".into());
                            }
                            let p = match &a[0] {
                                Val::Str(s) => Path::new(s),
                                _ => return Err("path must be String".into()),
                            };
                            Val::Str(
                                p.extension()
                                    .map(|x| x.to_string_lossy().to_string())
                                    .unwrap_or_default(),
                            )
                        }
                        "copy_file" => {
                            if a.len() != 2 {
                                return Err("copy_file expects source,destination".into());
                            }
                            let src = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("source must be String".into()),
                            };
                            let dst = match &a[1] {
                                Val::Str(s) => s,
                                _ => return Err("destination must be String".into()),
                            };
                            Val::Num(fs::copy(src, dst).map_err(|e| e.to_string())? as f64)
                        }
                        "move_file" => {
                            if a.len() != 2 {
                                return Err("move_file expects source,destination".into());
                            }
                            let src = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("source must be String".into()),
                            };
                            let dst = match &a[1] {
                                Val::Str(s) => s,
                                _ => return Err("destination must be String".into()),
                            };
                            fs::rename(src, dst).map_err(|e| e.to_string())?;
                            Val::Null
                        }
                        "file_size" => {
                            if a.len() != 1 {
                                return Err("file_size expects path".into());
                            }
                            let p = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("path must be String".into()),
                            };
                            Val::Num(fs::metadata(p).map_err(|e| e.to_string())?.len() as f64)
                        }
                        "is_file" => {
                            if a.len() != 1 {
                                return Err("is_file expects path".into());
                            }
                            let p = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("path must be String".into()),
                            };
                            Val::Bool(Path::new(p).is_file())
                        }
                        "is_dir" => {
                            if a.len() != 1 {
                                return Err("is_dir expects path".into());
                            }
                            let p = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("path must be String".into()),
                            };
                            Val::Bool(Path::new(p).is_dir())
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
                        "uuid_v4" => {
                            if !a.is_empty() {
                                return Err("uuid_v4 expects no arguments".into());
                            }
                            Val::Str(uuid::Uuid::new_v4().to_string())
                        }
                        "base64_encode" => {
                            if a.len() != 1 {
                                return Err("base64_encode expects value".into());
                            }
                            Val::Str(
                                base64::engine::general_purpose::STANDARD
                                    .encode(a[0].show().as_bytes()),
                            )
                        }
                        "base64_decode" => {
                            if a.len() != 1 {
                                return Err("base64_decode expects text".into());
                            }
                            let text = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("text must be String".into()),
                            };
                            let bytes = base64::engine::general_purpose::STANDARD
                                .decode(text)
                                .map_err(|e| e.to_string())?;
                            Val::Str(String::from_utf8(bytes).map_err(|e| e.to_string())?)
                        }
                        "toml_parse" => {
                            if a.len() != 1 {
                                return Err("toml_parse expects text".into());
                            }
                            let text = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("text must be String".into()),
                            };
                            let parsed: toml::Value =
                                toml::from_str(text).map_err(|e| e.to_string())?;
                            let json = serde_json::to_value(parsed).map_err(|e| e.to_string())?;
                            json_to_val(&json)
                        }
                        "toml_stringify" => {
                            if a.len() != 1 {
                                return Err("toml_stringify expects value".into());
                            }
                            Val::Str(
                                toml::to_string(&val_to_json(&a[0])).map_err(|e| e.to_string())?,
                            )
                        }
                        "now_utc" => {
                            if !a.is_empty() {
                                return Err("now_utc expects no arguments".into());
                            }
                            Val::Str(chrono::Utc::now().to_rfc3339())
                        }
                        "now_local" => {
                            if !a.is_empty() {
                                return Err("now_local expects no arguments".into());
                            }
                            Val::Str(chrono::Local::now().to_rfc3339())
                        }
                        "datetime_format" => {
                            if a.len() != 2 {
                                return Err("datetime_format expects timestamp,format".into());
                            }
                            let timestamp = match a[0] {
                                Val::Num(n) => n,
                                _ => return Err("timestamp must be Number".into()),
                            };
                            let format = match &a[1] {
                                Val::Str(s) => s,
                                _ => return Err("format must be String".into()),
                            };
                            let secs = timestamp.trunc() as i64;
                            let nanos = ((timestamp.fract().abs()) * 1_000_000_000.0) as u32;
                            let dt = chrono::DateTime::<chrono::Utc>::from_timestamp(secs, nanos)
                                .ok_or("invalid timestamp")?;
                            Val::Str(dt.format(format).to_string())
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
                        "hmac_sha256" => {
                            if a.len() != 2 {
                                return Err("hmac_sha256 expects key,message".into());
                            }
                            let key = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("key must be String".into()),
                            };
                            let message = match &a[1] {
                                Val::Str(s) => s,
                                _ => return Err("message must be String".into()),
                            };
                            Val::Str(hmac_sha256_hex(key, message))
                        }
                        "pbkdf2_sha256" => {
                            if a.len() != 4 {
                                return Err(
                                    "pbkdf2_sha256 expects password,salt,iterations,bytes".into()
                                );
                            }
                            let password = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("password must be String".into()),
                            };
                            let salt = match &a[1] {
                                Val::Str(s) => s,
                                _ => return Err("salt must be String".into()),
                            };
                            let iterations = match a[2] {
                                Val::Num(n) => n.max(1.0) as u32,
                                _ => return Err("iterations must be Number".into()),
                            };
                            let bytes = match a[3] {
                                Val::Num(n) => n.max(1.0) as usize,
                                _ => return Err("bytes must be Number".into()),
                            };
                            Val::Str(pbkdf2_sha256_hex(password, salt, iterations, bytes))
                        }
                        "constant_time_equal" => {
                            if a.len() != 2 {
                                return Err("constant_time_equal expects two strings".into());
                            }
                            let left = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("left must be String".into()),
                            };
                            let right = match &a[1] {
                                Val::Str(s) => s,
                                _ => return Err("right must be String".into()),
                            };
                            Val::Bool(constant_time_strings_equal(left, right))
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
                        #[cfg(feature = "native-services")]
                        "http_request" => {
                            if a.len() != 5 {
                                return Err(
                                    "http_request expects method,url,headers,body,timeout_ms"
                                        .into(),
                                );
                            }
                            let method_text = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("method must be String".into()),
                            };
                            let url = match &a[1] {
                                Val::Str(s) => s,
                                _ => return Err("url must be String".into()),
                            };
                            let headers = match &a[2] {
                                Val::Map(p) => p,
                                _ => return Err("headers must be Map".into()),
                            };
                            let timeout = match a[4] {
                                Val::Num(n) => n.max(1.0) as u64,
                                _ => return Err("timeout_ms must be Number".into()),
                            };
                            let client = reqwest::blocking::Client::builder()
                                .timeout(std::time::Duration::from_millis(timeout))
                                .build()
                                .map_err(|e| e.to_string())?;
                            let method = reqwest::Method::from_bytes(method_text.as_bytes())
                                .map_err(|e| e.to_string())?;
                            let mut request = client.request(method, url);
                            for (k, v) in headers {
                                request = request.header(k.show(), v.show());
                            }
                            if !matches!(a[3], Val::Null) {
                                let body = match &a[3] {
                                    Val::Str(s) => s.clone(),
                                    other => json_stringify_value(other)?,
                                };
                                request = request.body(body);
                            }
                            let response = request.send().map_err(|e| e.to_string())?;
                            let status = response.status();
                            let header_pairs = response
                                .headers()
                                .iter()
                                .map(|(k, v)| {
                                    (
                                        Val::Str(k.as_str().to_string()),
                                        Val::Str(v.to_str().unwrap_or("").to_string()),
                                    )
                                })
                                .collect::<Vec<_>>();
                            let body = response.text().map_err(|e| e.to_string())?;
                            Val::Map(vec![
                                (Val::Str("status".into()), Val::Num(status.as_u16() as f64)),
                                (Val::Str("ok".into()), Val::Bool(status.is_success())),
                                (Val::Str("headers".into()), Val::Map(header_pairs)),
                                (Val::Str("body".into()), Val::Str(body)),
                            ])
                        }
                        #[cfg(feature = "native-services")]
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
                        #[cfg(feature = "native-services")]
                        "http_post" => {
                            if a.len() != 2 {
                                return Err("http_post expects url,body".into());
                            }
                            let url = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("url must be String".into()),
                            };
                            let body = match &a[1] {
                                Val::Str(s) => s.clone(),
                                other => json_stringify_value(other)?,
                            };
                            let text = reqwest::blocking::Client::new()
                                .post(url)
                                .body(body)
                                .send()
                                .map_err(|e| e.to_string())?
                                .text()
                                .map_err(|e| e.to_string())?;
                            Val::Str(text)
                        }
                        "sleep_ms" => {
                            if a.len() != 1 {
                                return Err("sleep_ms expects milliseconds".into());
                            }
                            let ms = match a[0] {
                                Val::Num(n) => n.max(0.0) as u64,
                                _ => return Err("milliseconds must be Number".into()),
                            };
                            thread::sleep(std::time::Duration::from_millis(ms));
                            Val::Null
                        }
                        "cpu_count" => Val::Num(
                            thread::available_parallelism()
                                .map(|n| n.get())
                                .unwrap_or(1) as f64,
                        ),
                        "platform_os" => Val::Str(env::consts::OS.to_string()),
                        "platform_arch" => Val::Str(env::consts::ARCH.to_string()),
                        "platform_family" => Val::Str(env::consts::FAMILY.to_string()),
                        "path_separator" => Val::Str(std::path::MAIN_SEPARATOR.to_string()),
                        #[cfg(feature = "native-services")]
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
                        #[cfg(feature = "native-services")]
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
                        #[cfg(feature = "native-services")]
                        "sqlite_exec_params" => {
                            if a.len() != 3 {
                                return Err("sqlite_exec_params expects path,sql,params".into());
                            }
                            let path = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("path must be String".into()),
                            };
                            let sql = match &a[1] {
                                Val::Str(s) => s,
                                _ => return Err("sql must be String".into()),
                            };
                            let params = match &a[2] {
                                Val::List(v) => v,
                                _ => return Err("params must be List".into()),
                            };
                            let values: Vec<rusqlite::types::Value> =
                                params.iter().map(val_to_sql_value).collect();
                            let conn = Connection::open(path).map_err(|e| e.to_string())?;
                            let n = conn
                                .execute(sql, rusqlite::params_from_iter(values.iter()))
                                .map_err(|e| e.to_string())?;
                            Val::Num(n as f64)
                        }
                        #[cfg(feature = "native-services")]
                        "sqlite_query_params" => {
                            if a.len() != 3 {
                                return Err("sqlite_query_params expects path,sql,params".into());
                            }
                            let path = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("path must be String".into()),
                            };
                            let sql = match &a[1] {
                                Val::Str(s) => s,
                                _ => return Err("sql must be String".into()),
                            };
                            let params = match &a[2] {
                                Val::List(v) => v,
                                _ => return Err("params must be List".into()),
                            };
                            let values: Vec<rusqlite::types::Value> =
                                params.iter().map(val_to_sql_value).collect();
                            let conn = Connection::open(path).map_err(|e| e.to_string())?;
                            let mut stmt = conn.prepare(sql).map_err(|e| e.to_string())?;
                            let names: Vec<String> =
                                stmt.column_names().iter().map(|s| s.to_string()).collect();
                            let rows = stmt
                                .query_map(rusqlite::params_from_iter(values.iter()), |row| {
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
                        "spawn" => {
                            if a.len() != 2 {
                                return Err("spawn expects callable,args".into());
                            }
                            let (name, def) = self.callable_def(&a[0])?;
                            let args = match &a[1] {
                                Val::List(v) => v.clone(),
                                _ => return Err("spawn args must be List".into()),
                            };
                            if def.params.len() != args.len() {
                                return Err(format!(
                                    "{name} expects {} arguments, got {}",
                                    def.params.len(),
                                    args.len()
                                ));
                            }
                            for ((param, ty), value) in def.params.iter().zip(args.iter()) {
                                if let Some(expected) = ty {
                                    if !self.value_matches(value, expected) {
                                        return Err(format!(
                                            "argument '{param}' expected {expected}, got {}",
                                            value.show()
                                        ));
                                    }
                                }
                            }
                            let funcs = self.funcs.clone();
                            let classes = self.classes.clone();
                            let objects = self.objects.clone();
                            let id = TASK_IDS.fetch_add(1, Ordering::Relaxed);
                            let handle = thread::spawn(move || {
                                let mut vars = def.captures.clone();
                                let mut var_types = def.capture_types.clone();
                                let return_type = def.return_type.clone();
                                for ((param, ty), value) in
                                    def.params.into_iter().zip(args.into_iter())
                                {
                                    if let Some(expected) = ty {
                                        var_types.insert(param.clone(), expected);
                                    }
                                    vars.insert(param, value);
                                }
                                let mut nested =
                                    Vm::nested(def.code, vars, funcs, classes, objects);
                                nested.var_types = var_types;
                                let result = nested.execute()?;
                                if let Some(expected) = return_type {
                                    if !nested.value_matches(&result, &expected) {
                                        return Err(format!(
                                            "task return value expected {expected}, got {}",
                                            result.show()
                                        ));
                                    }
                                }
                                Ok(result)
                            });
                            tasks()
                                .lock()
                                .map_err(|_| "task registry poisoned")?
                                .insert(id, handle);
                            Val::Num(id as f64)
                        }
                        "task_done" => {
                            if a.len() != 1 {
                                return Err("task_done expects task id".into());
                            }
                            let id = match a[0] {
                                Val::Num(n) => n as u64,
                                _ => return Err("task id must be Number".into()),
                            };
                            let guard = tasks().lock().map_err(|_| "task registry poisoned")?;
                            Val::Bool(guard.get(&id).map(|h| h.is_finished()).unwrap_or(true))
                        }
                        "task_status" => {
                            if a.len() != 1 {
                                return Err("task_status expects task id".into());
                            }
                            let id = match a[0] {
                                Val::Num(n) => n as u64,
                                _ => return Err("task id must be Number".into()),
                            };
                            let guard = tasks().lock().map_err(|_| "task registry poisoned")?;
                            Val::Str(
                                match guard.get(&id) {
                                    Some(h) if h.is_finished() => "finished",
                                    Some(_) => "running",
                                    None => "consumed",
                                }
                                .into(),
                            )
                        }
                        "await_timeout" => {
                            if a.len() != 2 {
                                return Err("await_timeout expects task_id,milliseconds".into());
                            }
                            let id = match a[0] {
                                Val::Num(n) => n as u64,
                                _ => return Err("task id must be Number".into()),
                            };
                            let ms = match a[1] {
                                Val::Num(n) if n >= 0.0 => n as u64,
                                _ => return Err("timeout must be a non-negative Number".into()),
                            };
                            let deadline = Instant::now() + Duration::from_millis(ms);
                            loop {
                                let finished = {
                                    let guard =
                                        tasks().lock().map_err(|_| "task registry poisoned")?;
                                    match guard.get(&id) {
                                        Some(h) => h.is_finished(),
                                        None => return Err(format!("unknown task {id}")),
                                    }
                                };
                                if finished {
                                    let handle = tasks()
                                        .lock()
                                        .map_err(|_| "task registry poisoned")?
                                        .remove(&id)
                                        .ok_or(format!("unknown task {id}"))?;
                                    break handle
                                        .join()
                                        .map_err(|_| "task panicked".to_string())??;
                                }
                                if Instant::now() >= deadline {
                                    break Val::Null;
                                }
                                thread::sleep(Duration::from_millis(1));
                            }
                        }
                        "await_task" => {
                            if a.len() != 1 {
                                return Err("await_task expects task id".into());
                            }
                            let id = match a[0] {
                                Val::Num(n) => n as u64,
                                _ => return Err("task id must be Number".into()),
                            };
                            let handle = tasks()
                                .lock()
                                .map_err(|_| "task registry poisoned")?
                                .remove(&id)
                                .ok_or(format!("unknown task {id}"))?;
                            handle.join().map_err(|_| "task panicked".to_string())??
                        }
                        "gather" => {
                            if a.len() != 1 {
                                return Err("gather expects list of task ids".into());
                            }
                            let ids = match &a[0] {
                                Val::List(v) => v.clone(),
                                _ => return Err("gather expects List".into()),
                            };
                            let mut out = vec![];
                            for task in ids {
                                let id = match task {
                                    Val::Num(n) => n as u64,
                                    _ => return Err("gather task ids must be Numbers".into()),
                                };
                                let handle = tasks()
                                    .lock()
                                    .map_err(|_| "task registry poisoned")?
                                    .remove(&id)
                                    .ok_or(format!("unknown task {id}"))?;
                                out.push(handle.join().map_err(|_| "task panicked".to_string())??);
                            }
                            Val::List(out)
                        }
                        "channel" => {
                            if !a.is_empty() {
                                return Err("channel expects no arguments".into());
                            }
                            let (tx, rx) = mpsc::channel::<Val>();
                            let id = CHANNEL_IDS.fetch_add(1, Ordering::Relaxed);
                            channels()
                                .lock()
                                .map_err(|_| "channel registry poisoned")?
                                .insert(id, (tx, Arc::new(Mutex::new(rx))));
                            Val::Num(id as f64)
                        }
                        "send" => {
                            if a.len() != 2 {
                                return Err("send expects channel,value".into());
                            }
                            let id = match a[0] {
                                Val::Num(n) => n as u64,
                                _ => return Err("channel id must be Number".into()),
                            };
                            let tx = channels()
                                .lock()
                                .map_err(|_| "channel registry poisoned")?
                                .get(&id)
                                .map(|(tx, _)| tx.clone())
                                .ok_or(format!("unknown channel {id}"))?;
                            tx.send(a[1].clone()).map_err(|e| e.to_string())?;
                            Val::Null
                        }
                        "recv" => {
                            if a.len() != 1 {
                                return Err("recv expects channel".into());
                            }
                            let id = match a[0] {
                                Val::Num(n) => n as u64,
                                _ => return Err("channel id must be Number".into()),
                            };
                            let rx = channels()
                                .lock()
                                .map_err(|_| "channel registry poisoned")?
                                .get(&id)
                                .map(|(_, rx)| rx.clone())
                                .ok_or(format!("unknown channel {id}"))?;
                            let value = rx
                                .lock()
                                .map_err(|_| "channel receiver poisoned")?
                                .recv()
                                .map_err(|e| e.to_string())?;
                            value
                        }
                        "try_recv" => {
                            if a.len() != 1 {
                                return Err("try_recv expects channel".into());
                            }
                            let id = match a[0] {
                                Val::Num(n) => n as u64,
                                _ => return Err("channel id must be Number".into()),
                            };
                            let rx = channels()
                                .lock()
                                .map_err(|_| "channel registry poisoned")?
                                .get(&id)
                                .map(|(_, rx)| rx.clone())
                                .ok_or(format!("unknown channel {id}"))?;
                            match rx
                                .lock()
                                .map_err(|_| "channel receiver poisoned")?
                                .try_recv()
                            {
                                Ok(v) => v,
                                Err(TryRecvError::Empty) => Val::Null,
                                Err(TryRecvError::Disconnected) => {
                                    return Err("channel disconnected".into());
                                }
                            }
                        }
                        "ffi_available" => {
                            if a.len() != 1 {
                                return Err("ffi_available expects library path".into());
                            }
                            let path = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("library path must be String".into()),
                            };
                            Val::Bool(unsafe { Library::new(path) }.is_ok())
                        }
                        "ffi_i32" => {
                            if a.len() != 3 {
                                return Err("ffi_i32 expects library,symbol,args".into());
                            }
                            let path = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("library path must be String".into()),
                            };
                            let symbol = match &a[1] {
                                Val::Str(s) => s,
                                _ => return Err("symbol must be String".into()),
                            };
                            let vals = match &a[2] {
                                Val::List(v) => v,
                                _ => return Err("ffi args must be List".into()),
                            };
                            let nums: Result<Vec<i32>, String> = vals
                                .iter()
                                .map(|v| match v {
                                    Val::Num(n) => Ok(*n as i32),
                                    _ => Err("ffi_i32 arguments must be Numbers".into()),
                                })
                                .collect();
                            let nums = nums?;
                            let mut sym = symbol.as_bytes().to_vec();
                            sym.push(0);
                            let lib = unsafe { Library::new(path) }.map_err(|e| e.to_string())?;
                            let result: i32 = unsafe {
                                match nums.as_slice() {
                                    [] => {
                                        let f: libloading::Symbol<unsafe extern "C" fn() -> i32> =
                                            lib.get(&sym).map_err(|e| e.to_string())?;
                                        f()
                                    }
                                    [a] => {
                                        let f: libloading::Symbol<
                                            unsafe extern "C" fn(i32) -> i32,
                                        > = lib.get(&sym).map_err(|e| e.to_string())?;
                                        f(*a)
                                    }
                                    [a, b] => {
                                        let f: libloading::Symbol<
                                            unsafe extern "C" fn(i32, i32) -> i32,
                                        > = lib.get(&sym).map_err(|e| e.to_string())?;
                                        f(*a, *b)
                                    }
                                    [a, b, c] => {
                                        let f: libloading::Symbol<
                                            unsafe extern "C" fn(i32, i32, i32) -> i32,
                                        > = lib.get(&sym).map_err(|e| e.to_string())?;
                                        f(*a, *b, *c)
                                    }
                                    [a, b, c, e] => {
                                        let f: libloading::Symbol<
                                            unsafe extern "C" fn(i32, i32, i32, i32) -> i32,
                                        > = lib.get(&sym).map_err(|e| e.to_string())?;
                                        f(*a, *b, *c, *e)
                                    }
                                    _ => {
                                        return Err(
                                            "ffi_i32 supports 0..4 integer arguments".into()
                                        );
                                    }
                                }
                            };
                            Val::Num(result as f64)
                        }
                        "ffi_i64" => {
                            if a.len() != 3 {
                                return Err("ffi_i64 expects library,symbol,args".into());
                            }
                            let path = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("library path must be String".into()),
                            };
                            let symbol = match &a[1] {
                                Val::Str(s) => s,
                                _ => return Err("symbol must be String".into()),
                            };
                            let vals = match &a[2] {
                                Val::List(v) => v,
                                _ => return Err("ffi args must be List".into()),
                            };
                            let nums: Result<Vec<i64>, String> = vals
                                .iter()
                                .map(|v| match v {
                                    Val::Num(n) => Ok(*n as i64),
                                    _ => Err("ffi_i64 arguments must be Numbers".into()),
                                })
                                .collect();
                            let nums = nums?;
                            let mut sym = symbol.as_bytes().to_vec();
                            sym.push(0);
                            let lib = unsafe { Library::new(path) }.map_err(|e| e.to_string())?;
                            let result: i64 = unsafe {
                                match nums.as_slice() {
                                    [] => {
                                        let f: libloading::Symbol<unsafe extern "C" fn() -> i64> =
                                            lib.get(&sym).map_err(|e| e.to_string())?;
                                        f()
                                    }
                                    [a] => {
                                        let f: libloading::Symbol<
                                            unsafe extern "C" fn(i64) -> i64,
                                        > = lib.get(&sym).map_err(|e| e.to_string())?;
                                        f(*a)
                                    }
                                    [a, b] => {
                                        let f: libloading::Symbol<
                                            unsafe extern "C" fn(i64, i64) -> i64,
                                        > = lib.get(&sym).map_err(|e| e.to_string())?;
                                        f(*a, *b)
                                    }
                                    _ => {
                                        return Err(
                                            "ffi_i64 supports 0..2 integer arguments".into()
                                        );
                                    }
                                }
                            };
                            Val::Num(result as f64)
                        }
                        "ffi_f64" => {
                            if a.len() != 3 {
                                return Err("ffi_f64 expects library,symbol,args".into());
                            }
                            let path = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("library path must be String".into()),
                            };
                            let symbol = match &a[1] {
                                Val::Str(s) => s,
                                _ => return Err("symbol must be String".into()),
                            };
                            let vals = match &a[2] {
                                Val::List(v) => v,
                                _ => return Err("ffi args must be List".into()),
                            };
                            let nums: Result<Vec<f64>, String> = vals
                                .iter()
                                .map(|v| match v {
                                    Val::Num(n) => Ok(*n),
                                    _ => Err("ffi_f64 arguments must be Numbers".into()),
                                })
                                .collect();
                            let nums = nums?;
                            let mut sym = symbol.as_bytes().to_vec();
                            sym.push(0);
                            let lib = unsafe { Library::new(path) }.map_err(|e| e.to_string())?;
                            let result: f64 = unsafe {
                                match nums.as_slice() {
                                    [] => {
                                        let f: libloading::Symbol<unsafe extern "C" fn() -> f64> =
                                            lib.get(&sym).map_err(|e| e.to_string())?;
                                        f()
                                    }
                                    [a] => {
                                        let f: libloading::Symbol<
                                            unsafe extern "C" fn(f64) -> f64,
                                        > = lib.get(&sym).map_err(|e| e.to_string())?;
                                        f(*a)
                                    }
                                    [a, b] => {
                                        let f: libloading::Symbol<
                                            unsafe extern "C" fn(f64, f64) -> f64,
                                        > = lib.get(&sym).map_err(|e| e.to_string())?;
                                        f(*a, *b)
                                    }
                                    _ => {
                                        return Err(
                                            "ffi_f64 supports 0..2 floating arguments".into()
                                        );
                                    }
                                }
                            };
                            Val::Num(result)
                        }
                        _ => {
                            if self.classes.contains_key(&n) {
                                self.instantiate(&n, a)?
                            } else if let Some(Val::Function(target)) = self.vars.get(&n).cloned() {
                                let def = if let Some(def) = self.funcs.get(&target).cloned() {
                                    def
                                } else if let Some(def) = resolve_function_object(&target)? {
                                    def
                                } else {
                                    return Err(format!("dangling function reference '{target}'"));
                                };
                                self.call_def(def, a)?
                            } else {
                                let def = self
                                    .funcs
                                    .get(&n)
                                    .cloned()
                                    .ok_or(format!("unknown function or class '{n}'"))?;
                                self.call_def(def, a)?
                            }
                        }
                    };
                    self.stack.push(v);
                }
                Op::FunctionRef(name) => {
                    let def = self
                        .funcs
                        .get(&name)
                        .cloned()
                        .ok_or(format!("unknown function reference '{name}'"))?;
                    let key = export_function_object(def)?;
                    self.stack.push(Val::Function(key));
                }
                Op::Def(name, params, return_type, is_async, code) => {
                    self.funcs.insert(
                        name,
                        FunctionDef {
                            params,
                            return_type,
                            is_async,
                            code,
                            captures: self.vars.clone(),
                            capture_types: self.var_types.clone(),
                        },
                    );
                }
                Op::DefClass(name, parent, methods) => {
                    if let Some(ref p) = parent {
                        if !self.classes.contains_key(p) {
                            return Err(format!("unknown parent class '{p}'"));
                        }
                    }
                    let mut compiled = HashMap::new();
                    for (method, params, return_type, is_async, code) in methods {
                        compiled.insert(
                            method,
                            FunctionDef {
                                params,
                                return_type,
                                is_async,
                                code,
                                captures: self.vars.clone(),
                                capture_types: self.var_types.clone(),
                            },
                        );
                    }
                    self.classes.insert(
                        name,
                        ClassDef {
                            parent,
                            methods: compiled,
                        },
                    );
                }
                Op::Return => {
                    self.finished = true;
                    return self.pop();
                }
                Op::Yield => return self.pop(),
                Op::PushHandler(target, name) => self.handlers.push((target, name)),
                Op::PopHandler => {
                    let _ = self.handlers.pop();
                }
                Op::Throw => {
                    let value = self.pop()?;
                    if let Some((target, name)) = self.handlers.pop() {
                        self.vars.insert(name, value);
                        self.ip = target;
                    } else {
                        return Err(format!("uncaught throw: {}", value.show()));
                    }
                }
                Op::Halt => {
                    self.finished = true;
                    return Ok(Val::Null);
                }
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

fn semantic_check_ast(ast: &[Stmt]) -> Result<(), String> {
    fn known(scopes: &[HashSet<String>], name: &str) -> bool {
        scopes.iter().rev().any(|s| s.contains(name))
    }
    fn check_expr(e: &Expr, scopes: &[HashSet<String>], errors: &mut Vec<String>) {
        match e {
            Expr::Num(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Null => {}
            Expr::Var(n) => {
                if !known(scopes, n) {
                    errors.push(format!("undefined name '{n}'"));
                }
            }
            Expr::List(xs) => {
                for x in xs {
                    check_expr(x, scopes, errors)
                }
            }
            Expr::Map(xs) => {
                for (k, v) in xs {
                    check_expr(k, scopes, errors);
                    check_expr(v, scopes, errors)
                }
            }
            Expr::Index(a, b) | Expr::Bin(a, _, b) => {
                check_expr(a, scopes, errors);
                check_expr(b, scopes, errors)
            }
            Expr::Member(obj, _) => check_expr(obj, scopes, errors),
            Expr::MethodCall(obj, _, args) => {
                check_expr(obj, scopes, errors);
                for a in args {
                    check_expr(a, scopes, errors)
                }
            }
            Expr::Unary(_, x) | Expr::Await(x) => check_expr(x, scopes, errors),
            Expr::Lambda(params, _, body) => {
                let mut nested = scopes.to_vec();
                nested.push(params.iter().map(|(name, _)| name.clone()).collect());
                check_block(body, &nested, errors);
            }
            Expr::ListComp(value, var, iterable, condition) => {
                check_expr(iterable, scopes, errors);
                let mut nested = scopes.to_vec();
                nested.push(HashSet::from([var.clone()]));
                check_expr(value, &nested, errors);
                if let Some(cond) = condition {
                    check_expr(cond, &nested, errors);
                }
            }
            Expr::Call(_, args) => {
                for a in args {
                    check_expr(a, scopes, errors)
                }
            }
        }
    }
    fn check_block(stmts: &[Stmt], parent: &[HashSet<String>], errors: &mut Vec<String>) {
        let mut scopes = parent.to_vec();
        let mut local = HashSet::new();
        for s in stmts {
            match s {
                Stmt::Fn(n, _, _, _)
                | Stmt::AsyncFn(n, _, _, _)
                | Stmt::DecoratedFn(_, n, _, _, _)
                | Stmt::Class(n, _, _) => {
                    local.insert(n.clone());
                }
                _ => {}
            }
        }
        scopes.push(local);
        for s in stmts {
            match s {
                Stmt::Let(n, _, e, _) => {
                    check_expr(e, &scopes, errors);
                    let current = scopes.last_mut().unwrap();
                    if current.contains(n) {
                        errors.push(format!("duplicate declaration '{n}'"));
                    } else {
                        current.insert(n.clone());
                    }
                }
                Stmt::Assign(n, e) => {
                    if !known(&scopes, n) {
                        errors.push(format!("assignment to undefined name '{n}'"));
                    }
                    check_expr(e, &scopes, errors);
                }
                Stmt::Expr(e) | Stmt::Throw(e) | Stmt::Yield(e) => check_expr(e, &scopes, errors),
                Stmt::SetMember(obj, _, value) => {
                    check_expr(obj, &scopes, errors);
                    check_expr(value, &scopes, errors);
                }
                Stmt::If(c, a, b) => {
                    check_expr(c, &scopes, errors);
                    check_block(a, &scopes, errors);
                    check_block(b, &scopes, errors);
                }
                Stmt::While(c, b) => {
                    check_expr(c, &scopes, errors);
                    check_block(b, &scopes, errors);
                }
                Stmt::ForRange(n, a, b, body) => {
                    check_expr(a, &scopes, errors);
                    check_expr(b, &scopes, errors);
                    let mut nested = scopes.clone();
                    nested.push(HashSet::from([n.clone()]));
                    check_block(body, &nested, errors);
                }
                Stmt::ForEach(n, e, body) => {
                    check_expr(e, &scopes, errors);
                    let mut nested = scopes.clone();
                    nested.push(HashSet::from([n.clone()]));
                    check_block(body, &nested, errors);
                }
                Stmt::Match(subject, cases) => {
                    check_expr(subject, &scopes, errors);
                    for (pattern, body) in cases {
                        if let Some(pattern) = pattern {
                            check_expr(pattern, &scopes, errors);
                        }
                        check_block(body, &scopes, errors);
                    }
                }
                Stmt::Try(body, n, handler, finally) => {
                    check_block(body, &scopes, errors);
                    let mut nested = scopes.clone();
                    nested.push(HashSet::from([n.clone()]));
                    check_block(handler, &nested, errors);
                    check_block(finally, &scopes, errors);
                }
                Stmt::With(resource, name, body) => {
                    check_expr(resource, &scopes, errors);
                    let current = scopes.last_mut().unwrap();
                    if current.contains(name) {
                        errors.push(format!("duplicate declaration '{name}'"));
                    } else {
                        current.insert(name.clone());
                    }
                    check_block(body, &scopes, errors);
                }
                Stmt::Fn(_, params, _, body) | Stmt::AsyncFn(_, params, _, body) => {
                    let mut nested = scopes.clone();
                    nested.push(params.iter().map(|(name, _)| name.clone()).collect());
                    check_block(body, &nested, errors);
                }
                Stmt::DecoratedFn(decorator, _, params, _, body) => {
                    if !known(&scopes, decorator) {
                        errors.push(format!("undefined decorator '{decorator}'"));
                    }
                    let mut nested = scopes.clone();
                    nested.push(params.iter().map(|(name, _)| name.clone()).collect());
                    check_block(body, &nested, errors);
                }
                Stmt::Class(_, _, body) => {
                    for method in body {
                        match method {
                            Stmt::Fn(_, params, _, method_body)
                            | Stmt::AsyncFn(_, params, _, method_body) => {
                                let mut nested = scopes.clone();
                                nested.push(params.iter().map(|(name, _)| name.clone()).collect());
                                check_block(method_body, &nested, errors);
                            }
                            _ => {}
                        }
                    }
                }
                Stmt::Return(Some(e)) => check_expr(e, &scopes, errors),
                Stmt::Return(None) | Stmt::Break | Stmt::Continue => {}
            }
        }
    }
    let root = HashSet::new();
    let mut errors = vec![];
    check_block(ast, &[root], &mut errors);
    errors.sort();
    errors.dedup();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!("semantic errors: {}", errors.join("; ")))
    }
}

fn compile_source(src: &str) -> Result<Vec<Op>, String> {
    let t = lex(src)?;
    let ast = Parser::new(t).program()?;
    semantic_check_ast(&ast)?;
    let mut c = Compiler::new();
    c.emit_stmts(&ast)?;
    Ok(c.finish())
}
fn prepare_vm(src: &str) -> Result<Vm, String> {
    let mut vm = Vm::new(compile_source(src)?);
    let _ = vm.execute()?;
    Ok(vm)
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

fn debug_file(path: &Path) -> Result<(), String> {
    let src = load_source(path.to_string_lossy().as_ref())?;
    let code = compile_source(&src)?;
    let mut vm = Vm::new(code);
    vm.trace = true;
    vm.run()
}

fn ast_file(path: &Path) -> Result<(), String> {
    let src = load_source(path.to_string_lossy().as_ref())?;
    let ast = Parser::new(lex(&src)?).program()?;
    println!("{ast:#?}");
    Ok(())
}

fn symbols_file(path: &Path) -> Result<(), String> {
    let src = load_source(path.to_string_lossy().as_ref())?;
    let ast = Parser::new(lex(&src)?).program()?;
    println!("Symbols for {}", path.display());
    for stmt in ast {
        match stmt {
            Stmt::Let(name, ty, _, is_const) => {
                println!(
                    "  {} {}{}",
                    if is_const { "const" } else { "let" },
                    name,
                    ty.map(|t| format!(": {t}")).unwrap_or_default()
                );
            }
            Stmt::Fn(name, params, ret, _) | Stmt::AsyncFn(name, params, ret, _) => {
                let ps = params
                    .iter()
                    .map(|(n, t)| match t {
                        Some(t) => format!("{n}: {t}"),
                        None => n.clone(),
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                println!(
                    "  fn {name}({ps}){}",
                    ret.map(|r| format!(" -> {r}")).unwrap_or_default()
                );
            }
            Stmt::DecoratedFn(decorator, name, params, ret, _) => {
                let ps = params
                    .iter()
                    .map(|(n, t)| match t {
                        Some(t) => format!("{n}: {t}"),
                        None => n.clone(),
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                println!(
                    "  @{decorator} fn {name}({ps}){}",
                    ret.map(|r| format!(" -> {r}")).unwrap_or_default()
                );
            }
            Stmt::Class(name, parent, body) => {
                println!(
                    "  class {name}{}",
                    parent.map(|p| format!(": {p}")).unwrap_or_default()
                );
                for item in body {
                    match item {
                        Stmt::Fn(method, params, ret, _)
                        | Stmt::AsyncFn(method, params, ret, _) => {
                            let ps = params
                                .iter()
                                .map(|(n, t)| match t {
                                    Some(t) => format!("{n}: {t}"),
                                    None => n.clone(),
                                })
                                .collect::<Vec<_>>()
                                .join(", ");
                            println!(
                                "    method {method}({ps}){}",
                                ret.map(|r| format!(" -> {r}")).unwrap_or_default()
                            );
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn lint_file(path: &Path) -> Result<(), String> {
    let src = fs::read_to_string(path).map_err(|e| e.to_string())?;
    let loaded = load_source(path.to_string_lossy().as_ref())?;
    compile_source(&loaded)?;
    let mut issues = vec![];
    for (i, line) in src.lines().enumerate() {
        if line.contains('\t') {
            issues.push(format!("{}:{}: tab character", path.display(), i + 1));
        }
        if line.len() > 120 {
            issues.push(format!(
                "{}:{}: line exceeds 120 characters",
                path.display(),
                i + 1
            ));
        }
        if line.trim_end() != line {
            issues.push(format!("{}:{}: trailing whitespace", path.display(), i + 1));
        }
    }
    if issues.is_empty() {
        println!("{}: lint OK", path.display());
        Ok(())
    } else {
        for issue in &issues {
            println!("{issue}");
        }
        Err(format!("{} lint issue(s)", issues.len()))
    }
}

fn bench_file(path: &Path, iterations: usize) -> Result<(), String> {
    let src = load_source(path.to_string_lossy().as_ref())?;
    let code = compile_source(&src)?;
    let n = iterations.max(1);
    let mut times = Vec::with_capacity(n);
    for _ in 0..n {
        let start = Instant::now();
        Vm::new(code.clone()).run()?;
        times.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    let best = times.iter().copied().fold(f64::INFINITY, f64::min);
    let mean = times.iter().sum::<f64>() / times.len() as f64;
    println!(
        "{}: {n} run(s) | best={best:.3}ms | mean={mean:.3}ms | bytecode_ops={}",
        path.display(),
        code.len()
    );
    Ok(())
}

fn profile_file(path: &Path) -> Result<(), String> {
    let src = load_source(path.to_string_lossy().as_ref())?;
    let start_compile = Instant::now();
    let code = compile_source(&src)?;
    let compile_ms = start_compile.elapsed().as_secs_f64() * 1000.0;
    let start_run = Instant::now();
    Vm::new(code.clone()).run()?;
    let run_ms = start_run.elapsed().as_secs_f64() * 1000.0;
    println!("profile {}", path.display());
    println!("  source_bytes: {}", src.len());
    println!("  bytecode_ops: {}", code.len());
    println!("  compile_ms: {compile_ms:.3}");
    println!("  run_ms: {run_ms:.3}");
    Ok(())
}

fn doc_file(path: &Path) -> Result<(), String> {
    let src = fs::read_to_string(path).map_err(|e| e.to_string())?;
    let ast = Parser::new(lex(&src)?).program()?;
    let mut out = format!(
        "# API â€” {}\n\n",
        path.file_name()
            .and_then(|x| x.to_str())
            .unwrap_or("module")
    );
    for stmt in ast {
        match stmt {
            Stmt::Fn(name, params, return_type, _)
            | Stmt::AsyncFn(name, params, return_type, _) => {
                let rendered = params
                    .iter()
                    .map(|(n, t)| match t {
                        Some(t) => format!("{n}: {t}"),
                        None => n.clone(),
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let ret = return_type.unwrap_or_else(|| "Any".into());
                out.push_str(&format!("## fn {name}({rendered}) -> {ret}\n\n"));
            }
            Stmt::DecoratedFn(decorator, name, params, return_type, _) => {
                let rendered = params
                    .iter()
                    .map(|(n, t)| match t {
                        Some(t) => format!("{n}: {t}"),
                        None => n.clone(),
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let ret = return_type.unwrap_or_else(|| "Any".into());
                out.push_str(&format!(
                    "## @{decorator} fn {name}({rendered}) -> {ret}\n\n"
                ));
            }
            Stmt::Class(name, parent, body) => {
                match parent {
                    Some(p) => out.push_str(&format!("## class {name}: {p}\n\n")),
                    None => out.push_str(&format!("## class {name}\n\n")),
                }
                for item in body {
                    match item {
                        Stmt::Fn(method, params, return_type, _)
                        | Stmt::AsyncFn(method, params, return_type, _) => {
                            let rendered = params
                                .iter()
                                .map(|(n, t)| match t {
                                    Some(t) => format!("{n}: {t}"),
                                    None => n.clone(),
                                })
                                .collect::<Vec<_>>()
                                .join(", ");
                            let ret = return_type.unwrap_or_else(|| "Any".into());
                            out.push_str(&format!("- `{method}({rendered}) -> {ret}`\n"));
                        }
                        _ => {}
                    }
                }
                out.push('\n');
            }
            _ => {}
        }
    }
    let target = path.with_extension("api.md");
    fs::write(&target, out).map_err(|e| e.to_string())?;
    println!("Generated {}", target.display());
    Ok(())
}

fn lsp_word_at(text: &str, line: usize, character: usize) -> String {
    let Some(line_text) = text.lines().nth(line) else {
        return String::new();
    };
    let chars: Vec<char> = line_text.chars().collect();
    let pos = character.min(chars.len());
    let mut start = pos;
    while start > 0 && (chars[start - 1].is_ascii_alphanumeric() || chars[start - 1] == '_') {
        start -= 1;
    }
    let mut end = pos;
    while end < chars.len() && (chars[end].is_ascii_alphanumeric() || chars[end] == '_') {
        end += 1;
    }
    chars[start..end].iter().collect()
}

fn lsp_completion_items() -> JsonValue {
    let words = [
        "let",
        "const",
        "fn",
        "async",
        "class",
        "return",
        "if",
        "else",
        "while",
        "for",
        "in",
        "try",
        "catch",
        "finally",
        "throw",
        "with",
        "as",
        "match",
        "case",
        "break",
        "continue",
        "await",
        "yield",
        "gather",
        "true",
        "false",
        "null",
        "and",
        "or",
        "not",
        "print",
        "len",
        "str",
        "int",
        "float",
        "bool",
        "assert",
        "sum",
        "min",
        "max",
        "all",
        "any",
        "range",
        "keys",
        "values",
        "get",
        "type_of",
        "is_instance",
        "has_field",
        "append",
        "concat",
        "slice",
        "reverse",
        "sort",
        "unique",
        "enumerate",
        "zip",
        "map_call",
        "filter_call",
        "reduce_call",
        "spawn",
        "await_task",
        "task_done",
        "task_status",
        "await_timeout",
        "channel",
        "send",
        "recv",
        "try_recv",
        "http_get",
        "http_post",
        "json_parse",
        "json_stringify",
        "regex_findall",
        "sqlite_exec",
        "sqlite_query",
        "sha256",
        "vec_mean",
        "vec_std",
        "vec_median",
        "vec_percentile",
        "correlation",
        "matmul",
        "linreg_fit",
        "logreg_fit",
        "logreg_predict",
        "kmeans",
        "dense",
        "softmax",
        "tensor",
        "tensor_add",
        "tensor_sub",
        "tensor_mul",
        "tensor_div",
        "tensor_neg",
        "tensor_pow",
        "tensor_matmul",
        "tensor_softmax",
        "tensor_relu",
        "tensor_sigmoid",
        "tensor_tanh",
        "tensor_exp",
        "tensor_log",
        "tensor_reshape",
        "tensor_transpose",
        "tensor_detach",
        "tensor_numel",
        "tensor_backward",
        "tensor_step",
        "tensor_adam_step",
        "tensor_reset_optimizer",
        "tensor_zero_grad",
        "csv_read",
        "csv_write",
        "table_select",
        "table_inner_join",
        "plot_svg",
        "ffi_available",
        "ffi_i32",
        "ffi_i64",
        "ffi_f64",
    ];
    JsonValue::Array(
        words.iter().map(|w| serde_json::json!({
            "label": w,
            "kind": if ["let","const","fn","class","return","if","else","while","for","in","try","catch","throw","match","case","break","continue","await"].contains(w) { 14 } else { 3 },
            "detail": "Veyra"
        })).collect()
    )
}

fn lsp_diagnostics(text: &str) -> Vec<JsonValue> {
    let has_imports = text
        .lines()
        .any(|line| line.trim_start().starts_with("use "));
    let filtered = text
        .lines()
        .map(|line| {
            if line.trim_start().starts_with("use ") {
                ""
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let result = match lex(&filtered) {
        Err(e) => Err(e),
        Ok(tokens) => match Parser::new(tokens).program() {
            Err(e) => Err(e),
            Ok(ast) => {
                if has_imports {
                    Ok(())
                } else {
                    semantic_check_ast(&ast)
                }
            }
        },
    };
    match result {
        Ok(()) => vec![],
        Err(message) => vec![serde_json::json!({
            "range": {
                "start": {"line":0,"character":0},
                "end": {"line":0,"character":1}
            },
            "severity": 1,
            "source": "veyra",
            "message": message
        })],
    }
}

fn lsp_publish_diagnostics(
    connection: &lsp_server::Connection,
    uri: &str,
    text: &str,
) -> Result<(), String> {
    let notification = lsp_server::Notification {
        method: "textDocument/publishDiagnostics".into(),
        params: serde_json::json!({
            "uri": uri,
            "diagnostics": lsp_diagnostics(text)
        }),
    };
    connection
        .sender
        .send(lsp_server::Message::Notification(notification))
        .map_err(|e| e.to_string())
}

fn lsp_definition(text: &str, uri: &str, word: &str) -> JsonValue {
    if word.is_empty() {
        return JsonValue::Null;
    }
    let patterns = [
        format!("fn {word}"),
        format!("class {word}"),
        format!("let {word}"),
        format!("const {word}"),
    ];
    for (line_no, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        if patterns.iter().any(|p| trimmed.starts_with(p)) {
            if let Some(col) = line.find(word) {
                return serde_json::json!({
                    "uri": uri,
                    "range": {
                        "start":{"line":line_no,"character":col},
                        "end":{"line":line_no,"character":col+word.chars().count()}
                    }
                });
            }
        }
    }
    JsonValue::Null
}

fn lsp_references(text: &str, uri: &str, word: &str) -> JsonValue {
    if word.is_empty() {
        return JsonValue::Array(vec![]);
    }
    let mut refs = vec![];
    for (line_no, line) in text.lines().enumerate() {
        let chars: Vec<char> = line.chars().collect();
        let target: Vec<char> = word.chars().collect();
        if target.is_empty() || chars.len() < target.len() {
            continue;
        }
        for i in 0..=chars.len() - target.len() {
            if chars[i..i + target.len()] != target[..] {
                continue;
            }
            let left_ok = i == 0 || !(chars[i - 1].is_ascii_alphanumeric() || chars[i - 1] == '_');
            let right = i + target.len();
            let right_ok = right == chars.len()
                || !(chars[right].is_ascii_alphanumeric() || chars[right] == '_');
            if left_ok && right_ok {
                refs.push(serde_json::json!({
                    "uri":uri,
                    "range":{
                        "start":{"line":line_no,"character":i},
                        "end":{"line":line_no,"character":right}
                    }
                }));
            }
        }
    }
    JsonValue::Array(refs)
}

fn lsp_rename(text: &str, uri: &str, word: &str, new_name: &str) -> JsonValue {
    if word.is_empty()
        || new_name.is_empty()
        || !new_name
            .chars()
            .enumerate()
            .all(|(i, c)| c == '_' || c.is_ascii_alphanumeric() && (i > 0 || !c.is_ascii_digit()))
    {
        return JsonValue::Null;
    }
    let refs = lsp_references(text, uri, word);
    let edits = refs
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|location| {
            location.get("range").cloned().map(|range| {
                serde_json::json!({
                    "range": range,
                    "newText": new_name
                })
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({"changes": {uri: edits}})
}

fn lsp_server() -> Result<(), String> {
    let (connection, io_threads) = lsp_server::Connection::stdio();
    let (initialize_id, _) = connection.initialize_start().map_err(|e| e.to_string())?;
    let initialize_result = serde_json::json!({
        "capabilities": {
            "textDocumentSync": 2,
            "completionProvider": {"resolveProvider": false, "triggerCharacters": ["."]},
            "hoverProvider": true,
            "definitionProvider": true,
            "referencesProvider": true,
            "renameProvider": true,
            "documentSymbolProvider": true,
            "documentFormattingProvider": true
        },
        "serverInfo": {"name": "Veyra Language Server", "version": VERSION}
    });
    connection
        .initialize_finish(initialize_id, initialize_result)
        .map_err(|e| e.to_string())?;

    let mut documents: HashMap<String, String> = HashMap::new();
    for message in &connection.receiver {
        match message {
            lsp_server::Message::Request(request) => {
                if connection
                    .handle_shutdown(&request)
                    .map_err(|e| e.to_string())?
                {
                    break;
                }
                let result = match request.method.as_str() {
                    "textDocument/completion" => lsp_completion_items(),
                    "textDocument/hover" => {
                        let uri = request
                            .params
                            .pointer("/textDocument/uri")
                            .and_then(JsonValue::as_str)
                            .unwrap_or("");
                        let line = request
                            .params
                            .pointer("/position/line")
                            .and_then(JsonValue::as_u64)
                            .unwrap_or(0) as usize;
                        let character = request
                            .params
                            .pointer("/position/character")
                            .and_then(JsonValue::as_u64)
                            .unwrap_or(0) as usize;
                        let word = documents
                            .get(uri)
                            .map(|text| lsp_word_at(text, line, character))
                            .unwrap_or_default();
                        if word.is_empty() {
                            JsonValue::Null
                        } else {
                            serde_json::json!({
                                "contents": {
                                    "kind": "markdown",
                                    "value": format!("Veyra symbol: {word}\n\nNative symbol or language construct.")
                                }
                            })
                        }
                    }
                    "textDocument/definition" => {
                        let uri = request
                            .params
                            .pointer("/textDocument/uri")
                            .and_then(JsonValue::as_str)
                            .unwrap_or("");
                        let line = request
                            .params
                            .pointer("/position/line")
                            .and_then(JsonValue::as_u64)
                            .unwrap_or(0) as usize;
                        let character = request
                            .params
                            .pointer("/position/character")
                            .and_then(JsonValue::as_u64)
                            .unwrap_or(0) as usize;
                        let text = documents.get(uri).cloned().unwrap_or_default();
                        let word = lsp_word_at(&text, line, character);
                        lsp_definition(&text, uri, &word)
                    }
                    "textDocument/references" => {
                        let uri = request
                            .params
                            .pointer("/textDocument/uri")
                            .and_then(JsonValue::as_str)
                            .unwrap_or("");
                        let line = request
                            .params
                            .pointer("/position/line")
                            .and_then(JsonValue::as_u64)
                            .unwrap_or(0) as usize;
                        let character = request
                            .params
                            .pointer("/position/character")
                            .and_then(JsonValue::as_u64)
                            .unwrap_or(0) as usize;
                        let text = documents.get(uri).cloned().unwrap_or_default();
                        let word = lsp_word_at(&text, line, character);
                        lsp_references(&text, uri, &word)
                    }
                    "textDocument/rename" => {
                        let uri = request
                            .params
                            .pointer("/textDocument/uri")
                            .and_then(JsonValue::as_str)
                            .unwrap_or("");
                        let line = request
                            .params
                            .pointer("/position/line")
                            .and_then(JsonValue::as_u64)
                            .unwrap_or(0) as usize;
                        let character = request
                            .params
                            .pointer("/position/character")
                            .and_then(JsonValue::as_u64)
                            .unwrap_or(0) as usize;
                        let new_name = request
                            .params
                            .get("newName")
                            .and_then(JsonValue::as_str)
                            .unwrap_or("");
                        let text = documents.get(uri).cloned().unwrap_or_default();
                        let word = lsp_word_at(&text, line, character);
                        lsp_rename(&text, uri, &word, new_name)
                    }
                    "textDocument/documentSymbol" => {
                        let uri = request
                            .params
                            .pointer("/textDocument/uri")
                            .and_then(JsonValue::as_str)
                            .unwrap_or("");
                        let text = documents.get(uri).cloned().unwrap_or_default();
                        let filtered = text
                            .lines()
                            .map(|line| {
                                if line.trim_start().starts_with("use ") {
                                    ""
                                } else {
                                    line
                                }
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        let mut symbols = vec![];
                        if let Ok(tokens) = lex(&filtered) {
                            if let Ok(ast) = Parser::new(tokens).program() {
                                for stmt in ast {
                                    let (name, kind) = match stmt {
                                        Stmt::Fn(name, _, _, _) | Stmt::AsyncFn(name, _, _, _) => {
                                            (name, 12)
                                        }
                                        Stmt::DecoratedFn(_, name, _, _, _) => (name, 12),
                                        Stmt::Class(name, _, _) => (name, 5),
                                        Stmt::Let(name, _, _, _) => (name, 13),
                                        _ => continue,
                                    };
                                    symbols.push(serde_json::json!({
                                        "name": name,
                                        "kind": kind,
                                        "range": {
                                            "start":{"line":0,"character":0},
                                            "end":{"line":0,"character":1}
                                        },
                                        "selectionRange": {
                                            "start":{"line":0,"character":0},
                                            "end":{"line":0,"character":1}
                                        }
                                    }));
                                }
                            }
                        }
                        JsonValue::Array(symbols)
                    }
                    "textDocument/formatting" => {
                        let uri = request
                            .params
                            .pointer("/textDocument/uri")
                            .and_then(JsonValue::as_str)
                            .unwrap_or("");
                        let text = documents.get(uri).cloned().unwrap_or_default();
                        let formatted = fmt_source(&text);
                        let lines = text.lines().collect::<Vec<_>>();
                        let end_line = lines.len().saturating_sub(1);
                        let end_char = lines.last().map(|x| x.chars().count()).unwrap_or(0);
                        serde_json::json!([{
                            "range": {
                                "start":{"line":0,"character":0},
                                "end":{"line":end_line,"character":end_char}
                            },
                            "newText": formatted
                        }])
                    }
                    _ => JsonValue::Null,
                };
                let response = lsp_server::Response::new_ok(request.id.clone(), result);
                connection
                    .sender
                    .send(lsp_server::Message::Response(response))
                    .map_err(|e| e.to_string())?;
            }
            lsp_server::Message::Notification(notification) => match notification.method.as_str() {
                "textDocument/didOpen" => {
                    let uri = notification
                        .params
                        .pointer("/textDocument/uri")
                        .and_then(JsonValue::as_str)
                        .unwrap_or("")
                        .to_string();
                    let text = notification
                        .params
                        .pointer("/textDocument/text")
                        .and_then(JsonValue::as_str)
                        .unwrap_or("")
                        .to_string();
                    documents.insert(uri.clone(), text.clone());
                    lsp_publish_diagnostics(&connection, &uri, &text)?;
                }
                "textDocument/didChange" => {
                    let uri = notification
                        .params
                        .pointer("/textDocument/uri")
                        .and_then(JsonValue::as_str)
                        .unwrap_or("")
                        .to_string();
                    if let Some(text) = notification
                        .params
                        .pointer("/contentChanges/0/text")
                        .and_then(JsonValue::as_str)
                    {
                        documents.insert(uri.clone(), text.to_string());
                        lsp_publish_diagnostics(&connection, &uri, text)?;
                    }
                }
                "textDocument/didClose" => {
                    let uri = notification
                        .params
                        .pointer("/textDocument/uri")
                        .and_then(JsonValue::as_str)
                        .unwrap_or("");
                    documents.remove(uri);
                    lsp_publish_diagnostics(&connection, uri, "")?;
                }
                _ => {}
            },
            lsp_server::Message::Response(_) => {}
        }
    }
    io_threads.join().map_err(|e| e.to_string())?;
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
    if let Ok(url) = env::var("VEYRA_REGISTRY_URL") {
        #[cfg(feature = "native-services")]
        {
            let response = reqwest::blocking::get(&url).map_err(|e| e.to_string())?;
            if !response.status().is_success() {
                return Err(format!(
                    "remote registry returned HTTP {}",
                    response.status()
                ));
            }
            let text = response.text().map_err(|e| e.to_string())?;
            let json = serde_json::from_str(&text).map_err(|e| e.to_string())?;
            return Ok((PathBuf::from(url), json));
        }
        #[cfg(not(feature = "native-services"))]
        {
            return Err(format!(
                "remote registry '{url}' requires the native-services feature"
            ));
        }
    }
    let path = veyra_home().join("registry").join("index.json");
    let text = fs::read_to_string(&path)
        .map_err(|e| format!("registry unavailable at {}: {e}", path.display()))?;
    let json = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    Ok((path, json))
}

fn safe_package_relative_path(value: &str) -> Result<PathBuf, String> {
    let path = Path::new(value);
    if path.is_absolute()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(format!("unsafe package path '{value}'"));
    }
    Ok(path.to_path_buf())
}

#[cfg(feature = "native-services")]
fn fetch_package_file(url: &str) -> Result<Vec<u8>, String> {
    let response = reqwest::blocking::get(url).map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!(
            "package download returned HTTP {}",
            response.status()
        ));
    }
    response
        .bytes()
        .map(|b| b.to_vec())
        .map_err(|e| e.to_string())
}

fn hash_directory(dir: &Path) -> Result<String, String> {
    fn collect(base: &Path, dir: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
        for entry in fs::read_dir(dir).map_err(|e| e.to_string())? {
            let path = entry.map_err(|e| e.to_string())?.path();
            if path.is_dir() {
                collect(base, &path, files)?;
            } else {
                files.push(path);
            }
        }
        Ok(())
    }
    if !dir.exists() {
        return Err(format!("missing directory {}", dir.display()));
    }
    let mut files = vec![];
    collect(dir, dir, &mut files)?;
    files.sort();
    let mut h = Sha256::new();
    for path in files {
        let rel = path
            .strip_prefix(dir)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        h.update(rel.as_bytes());
        h.update([0]);
        h.update(fs::read(&path).map_err(|e| e.to_string())?);
        h.update([0]);
    }
    Ok(format!("{:x}", h.finalize()))
}

fn write_package_lock(root: &Path, manifest: &JsonValue) -> Result<(), String> {
    let mut locked = serde_json::Map::new();
    if let Some(packages) = manifest.get("packages").and_then(JsonValue::as_object) {
        for (name, meta) in packages {
            let mut entry = meta.clone();
            let hash = hash_directory(&root.join("vendor").join(name))?;
            entry["sha256"] = JsonValue::String(hash);
            locked.insert(name.clone(), entry);
        }
    }
    let lock = serde_json::json!({
        "format": 2,
        "engine": VERSION,
        "packages": JsonValue::Object(locked)
    });
    fs::write(
        root.join("veyra.lock"),
        serde_json::to_string_pretty(&lock).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())
}

fn pkg_add_inner(name: &str, seen: &mut HashSet<String>) -> Result<(), String> {
    if !seen.insert(name.to_string()) {
        return Ok(());
    }
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
    if let Some(dependencies) = pkg.get("dependencies").and_then(JsonValue::as_array) {
        for dependency in dependencies {
            let dependency = dependency
                .as_str()
                .ok_or("package dependency names must be Strings")?;
            pkg_add_inner(dependency, seen)?;
        }
    }
    let dest = root.join("vendor").join(name);
    if dest.exists() {
        fs::remove_dir_all(&dest).map_err(|e| e.to_string())?;
    }

    if let Some(files) = pkg.get("files").and_then(JsonValue::as_object) {
        fs::create_dir_all(&dest).map_err(|e| e.to_string())?;
        #[cfg(feature = "native-services")]
        {
            for (relative, url) in files {
                let relative = safe_package_relative_path(relative)?;
                let url = url
                    .as_str()
                    .ok_or("registry package file URL must be String")?;
                let target = dest.join(relative);
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                fs::write(&target, fetch_package_file(url)?).map_err(|e| e.to_string())?;
            }
        }
        #[cfg(not(feature = "native-services"))]
        {
            let _ = files;
            return Err("remote package files require the native-services feature".into());
        }
    } else if let Some(url) = pkg.get("url").and_then(JsonValue::as_str) {
        fs::create_dir_all(&dest).map_err(|e| e.to_string())?;
        let relative = safe_package_relative_path(entry)?;
        let target = dest.join(relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        #[cfg(feature = "native-services")]
        {
            fs::write(&target, fetch_package_file(url)?).map_err(|e| e.to_string())?;
        }
        #[cfg(not(feature = "native-services"))]
        {
            let _ = url;
            return Err("remote package download requires the native-services feature".into());
        }
    } else {
        let source = pkg
            .get("source")
            .and_then(JsonValue::as_str)
            .ok_or("registry package requires source, url, or files")?;
        let source_path = index_path.parent().unwrap_or(Path::new(".")).join(source);
        copy_dir_all(&source_path, &dest)?;
    }

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

fn pkg_add(name: &str) -> Result<(), String> {
    pkg_add_inner(name, &mut HashSet::new())
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

fn pkg_audit() -> Result<(), String> {
    let root = project_root(&env::current_dir().map_err(|e| e.to_string())?)?;
    let lock_path = root.join("veyra.lock");
    if !lock_path.exists() {
        return Err("veyra.lock not found; run veyra install/add first".into());
    }
    let lock: JsonValue =
        serde_json::from_str(&fs::read_to_string(&lock_path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let packages = lock
        .get("packages")
        .and_then(JsonValue::as_object)
        .ok_or("invalid lockfile packages")?;
    let mut checked = 0usize;
    for (name, meta) in packages {
        let expected = meta
            .get("sha256")
            .and_then(JsonValue::as_str)
            .ok_or(format!("package {name} has no sha256"))?;
        let actual = hash_directory(&root.join("vendor").join(name))?;
        if actual != expected {
            return Err(format!("package integrity mismatch: {name}"));
        }
        checked += 1;
    }
    println!("Audit OK: {checked} package(s), content hashes verified");
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

fn match_route_pattern(pattern: &str, path: &str) -> Option<Vec<(String, String)>> {
    let p: Vec<&str> = pattern
        .trim_matches('/')
        .split('/')
        .filter(|x| !x.is_empty())
        .collect();
    let a: Vec<&str> = path
        .trim_matches('/')
        .split('/')
        .filter(|x| !x.is_empty())
        .collect();
    if p.len() != a.len() {
        return None;
    }
    let mut params = vec![];
    for (expected, actual) in p.iter().zip(a.iter()) {
        if expected.starts_with('{') && expected.ends_with('}') && expected.len() > 2 {
            params.push((
                expected[1..expected.len() - 1].to_string(),
                (*actual).to_string(),
            ));
        } else if expected != actual {
            return None;
        }
    }
    Some(params)
}

fn query_params(url: &str) -> Val {
    let mut pairs = vec![];
    if let Some(query) = url.split_once('?').map(|(_, q)| q) {
        for item in query.split('&').filter(|x| !x.is_empty()) {
            let (key, value) = item.split_once('=').unwrap_or((item, ""));
            pairs.push((Val::Str(key.to_string()), Val::Str(value.to_string())));
        }
    }
    Val::Map(pairs)
}

fn serve_veyra_app(app_file: &Path, routes_file: &Path, port: u16) -> Result<(), String> {
    let src = load_source(app_file.to_string_lossy().as_ref())?;
    let vm = prepare_vm(&src)?;
    let config: JsonValue =
        serde_json::from_str(&fs::read_to_string(routes_file).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let routes = config
        .get("routes")
        .and_then(JsonValue::as_object)
        .ok_or("app routes file requires a 'routes' object")?;
    let server = Server::http(format!("0.0.0.0:{port}")).map_err(|e| e.to_string())?;
    println!("Veyra dynamic app: http://127.0.0.1:{port}");
    println!("Handlers: {}", routes.len());
    for mut req in server.incoming_requests() {
        let full_url = req.url().to_string();
        let path = full_url.split('?').next().unwrap_or("/").to_string();
        let method = req.method().as_str().to_string();
        let key = format!("{method} {path}");
        let mut route_params: Vec<(String, String)> = vec![];
        let mut handler = routes
            .get(&key)
            .or_else(|| routes.get(&path))
            .and_then(JsonValue::as_str)
            .map(str::to_string);
        if handler.is_none() {
            for (route_key, value) in routes {
                let (route_method, pattern) = route_key
                    .split_once(' ')
                    .map(|(m, p)| (Some(m), p))
                    .unwrap_or((None, route_key.as_str()));
                if route_method.is_some() && route_method != Some(method.as_str()) {
                    continue;
                }
                if let Some(params) = match_route_pattern(pattern, &path) {
                    if let Some(name) = value.as_str() {
                        handler = Some(name.to_string());
                        route_params = params;
                        break;
                    }
                }
            }
        }
        let response = if let Some(handler) = handler {
            let mut body = String::new();
            req.as_reader()
                .read_to_string(&mut body)
                .map_err(|e| e.to_string())?;
            let params = Val::Map(
                route_params
                    .iter()
                    .map(|(k, v)| (Val::Str(k.clone()), Val::Str(v.clone())))
                    .collect(),
            );
            let query = query_params(&full_url);
            let headers = Val::Map(
                req.headers()
                    .iter()
                    .map(|h| {
                        (
                            Val::Str(h.field.as_str().to_string()),
                            Val::Str(h.value.as_str().to_string()),
                        )
                    })
                    .collect(),
            );
            let request = Val::Map(vec![
                (Val::Str("method".into()), Val::Str(method)),
                (Val::Str("path".into()), Val::Str(path)),
                (Val::Str("url".into()), Val::Str(full_url)),
                (Val::Str("body".into()), Val::Str(body)),
                (Val::Str("params".into()), params),
                (Val::Str("query".into()), query),
                (Val::Str("headers".into()), headers),
            ]);
            match vm.invoke(&handler, vec![request]) {
                Ok(value) => {
                    let (status, content_type, body, response_headers) = match value {
                        Val::Map(pairs) => {
                            let status = map_lookup(&pairs, &Val::Str("status".into()))
                                .and_then(|v| {
                                    if let Val::Num(n) = v {
                                        Some(*n as u16)
                                    } else {
                                        None
                                    }
                                })
                                .unwrap_or(200);
                            let content_type = map_lookup(&pairs, &Val::Str("content_type".into()))
                                .and_then(|v| {
                                    if let Val::Str(s) = v {
                                        Some(s.clone())
                                    } else {
                                        None
                                    }
                                })
                                .unwrap_or_else(|| "application/json; charset=utf-8".into());
                            let body = map_lookup(&pairs, &Val::Str("body".into()))
                                .cloned()
                                .unwrap_or(Val::Null);
                            let text = match body {
                                Val::Str(s) => s,
                                other => serde_json::to_string(&val_to_json(&other))
                                    .map_err(|e| e.to_string())?,
                            };
                            let headers = match map_lookup(&pairs, &Val::Str("headers".into())) {
                                Some(Val::Map(items)) => items
                                    .iter()
                                    .filter_map(|(k, v)| match (k, v) {
                                        (Val::Str(name), Val::Str(value)) => {
                                            Some((name.clone(), value.clone()))
                                        }
                                        _ => None,
                                    })
                                    .collect::<Vec<_>>(),
                                _ => vec![],
                            };
                            (status, content_type, text, headers)
                        }
                        Val::Str(s) => (200, "text/plain; charset=utf-8".into(), s, vec![]),
                        other => (
                            200,
                            "application/json; charset=utf-8".into(),
                            serde_json::to_string(&val_to_json(&other))
                                .map_err(|e| e.to_string())?,
                            vec![],
                        ),
                    };
                    let mut r = Response::from_string(body).with_status_code(StatusCode(status));
                    if let Ok(h) = Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes())
                    {
                        r.add_header(h);
                    }
                    for (name, value) in response_headers {
                        if let Ok(h) = Header::from_bytes(name.as_bytes(), value.as_bytes()) {
                            r.add_header(h);
                        }
                    }
                    r
                }
                Err(e) => Response::from_string(format!("Veyra handler error: {e}"))
                    .with_status_code(StatusCode(500)),
            }
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
    println!("Veyra {VERSION} native REPL Ã¢â‚¬â€ :quit to exit");
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
        "Veyra {VERSION}\n\nUsage:\n  veyra run [file.vy]\n  veyra check [file.vy]\n  veyra lint [file.vy]\n  veyra debug [file.vy]\n  veyra ast [file.vy]\n  veyra symbols [file.vy]\n  veyra lsp\n  veyra test\n  veyra bench [file.vy] [iterations]\n  veyra profile [file.vy]\n  veyra doc [file.vy]\n  veyra build\n  veyra audit\n  veyra serve [dir] [port]\n  veyra web <routes.json> [port]\n  veyra app <app.vy> <routes.json> [port]\n  veyra disasm [file.vy]\n  veyra fmt <file.vy>\n  veyra new <name>\n  veyra pkg list\n  veyra pkg add <name>\n  veyra pkg remove <name>\n  veyra pkg installed\n  veyra doctor\n  veyra repl\n  veyra --version"
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
        Some("lint") => {
            let path = match a.get(2) {
                Some(p) => Ok(PathBuf::from(p)),
                None => project_entry(),
            };
            path.and_then(|p| lint_file(&p))
        }
        Some("debug") => {
            let path = match a.get(2) {
                Some(p) => Ok(PathBuf::from(p)),
                None => project_entry(),
            };
            path.and_then(|p| debug_file(&p))
        }
        Some("ast") => {
            let path = match a.get(2) {
                Some(p) => Ok(PathBuf::from(p)),
                None => project_entry(),
            };
            path.and_then(|p| ast_file(&p))
        }
        Some("symbols") => {
            let path = match a.get(2) {
                Some(p) => Ok(PathBuf::from(p)),
                None => project_entry(),
            };
            path.and_then(|p| symbols_file(&p))
        }
        Some("lsp") => lsp_server(),
        Some("test") => test_project(),
        Some("bench") => {
            let path = match a.get(2) {
                Some(p) => Ok(PathBuf::from(p)),
                None => project_entry(),
            };
            let iterations = a.get(3).and_then(|x| x.parse::<usize>().ok()).unwrap_or(10);
            path.and_then(|p| bench_file(&p, iterations))
        }
        Some("profile") => {
            let path = match a.get(2) {
                Some(p) => Ok(PathBuf::from(p)),
                None => project_entry(),
            };
            path.and_then(|p| profile_file(&p))
        }
        Some("doc") => {
            let path = match a.get(2) {
                Some(p) => Ok(PathBuf::from(p)),
                None => project_entry(),
            };
            path.and_then(|p| doc_file(&p))
        }
        Some("build") => build_project(),
        Some("audit") => pkg_audit(),
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
        Some("app") => {
            let app = a.get(2).ok_or("missing app.vy".into());
            let routes = a.get(3).ok_or("missing routes.json".into());
            match (app, routes) {
                (Ok(app), Ok(routes)) => {
                    let port = a.get(4).and_then(|x| x.parse::<u16>().ok()).unwrap_or(8080);
                    serve_veyra_app(Path::new(app), Path::new(routes), port)
                }
                (Err(e), _) | (_, Err(e)) => Err(e),
            }
        }
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
    #[test]
    fn general_iteration() {
        assert!(run_source("let total=0; for x in [1,2,3,4] { total=total+x; }").is_ok());
        assert!(run_source("let out=\"\"; for ch in \"abc\" { out=out+ch; }").is_ok());
    }
    #[test]
    fn explicit_exceptions() {
        assert!(
            run_source(
                "let caught=false; try { throw \"boom\"; } catch err { caught = err == \"boom\"; }"
            )
            .is_ok()
        );
        assert!(run_source("throw \"uncaught\";").is_err());
    }
    #[test]
    fn lexical_capture() {
        assert!(run_source("fn outer(x: Int) -> Int { let y=3; fn add(z: Int) -> Int { return y+z; } return add(x); } let result=outer(4);").is_ok());
    }
    #[test]
    fn native_tasks() {
        assert!(run_source("fn square(n: Int) -> Int { return n*n; } let task=spawn(\"square\",[9]); let result=await_task(task);").is_ok());
    }
    #[test]
    fn native_ffi_i32() {
        if cfg!(target_os = "windows") {
            assert!(run_source("let ok=ffi_available(\"msvcrt.dll\"); let x=ffi_i32(\"msvcrt.dll\",\"abs\",[-7]);").is_ok());
        }
    }
    #[test]
    fn native_classes_inheritance() {
        let src = "class Animal { fn init(self, name: String) -> Null { self.name = name; return null; } fn speak(self) -> String { return self.name; } } class Dog: Animal { fn rename(self, name: String) -> String { self.name = name; return self.name; } } let d=Dog(\"Rex\"); let a=d.speak(); let b=d.rename(\"Max\"); let c=d.name;";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn loop_control() {
        assert!(
            run_source(
                "let s=0; for n in range(0,10) { if n==2 { continue; } if n==5 { break; } s=s+n; }"
            )
            .is_ok()
        );
        assert!(compile_source("break;").is_err());
        assert!(compile_source("continue;").is_err());
    }
    #[test]
    fn runtime_type_enforcement() {
        assert!(run_source("let x: Int=1; x=2;").is_ok());
        assert!(run_source("let x: Int=1; x=\"bad\";").is_err());
        assert!(run_source("let x: Int=\"bad\";").is_err());
        assert!(run_source("fn f(x: Int)->Int{return x+1;} let y=f(3);").is_ok());
        assert!(run_source("fn f(x: Int)->Int{return x+1;} let y=f(\"bad\");").is_err());
        assert!(run_source("fn f()->Int{return \"bad\";} let y=f();").is_err());
        assert!(
            run_source(
                "class A {} class B: A {} fn use_a(x: A)->A{return x;} let b=B(); let x=use_a(b);"
            )
            .is_ok()
        );
        assert!(
            run_source(
                "class A {} class C {} fn use_a(x: A)->A{return x;} let c=C(); let x=use_a(c);"
            )
            .is_err()
        );
    }
    #[test]
    fn functional_collections() {
        let src = "fn twice(x: Int)->Int{return x*2;} fn even(x: Int)->Bool{return x%2==0;} fn add(a: Int,b: Int)->Int{return a+b;} let a=append([1,2],3); let b=slice(a,1,3); let c=reverse(b); let d=sort([3,1,2]); let e=unique([1,1,2,2,3]); let f=enumerate([7,8]); let g=zip([1,2],[3,4]); let h=map_call(\"twice\",[1,2,3]); let i=filter_call(\"even\",[1,2,3,4]); let j=reduce_call(\"add\",[1,2,3],0);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn everyday_builtins() {
        let src = "let a=int(\"7\"); let b=float(\"2.5\"); let c=bool(1); assert(a==7); let s=sum([1,2,3]); let lo=min([4,2,9]); let hi=max([4,2,9]); let aa=all([true,1,\"x\"]); let an=any([false,0,3]); let t=tuple([1,2]); let st=set([1,1,2]); let has=contains(st,2); let ix=index_of([9,8,7],8);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn expanded_science_data_ml() {
        let src = "let med=vec_median([1,2,9,4]); let p=vec_percentile([1,2,3,4],75); let c=correlation([1,2,3],[2,4,6]); let rows=[{\"id\":1,\"g\":\"a\"},{\"id\":2,\"g\":\"b\"},{\"id\":3,\"g\":\"a\"}]; let groups=table_group_count(rows,\"g\"); let sorted=table_sort(rows,\"id\"); let selected=table_select(rows,[\"id\"]); let joined=table_inner_join(rows,[{\"rid\":1,\"v\":9}],\"id\",\"rid\"); let model=logreg_fit([[0],[1],[2],[3]],[0,0,1,1],100,0.1); let prob=logreg_predict(model,[3]);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn channels_work() {
        assert!(run_source("let ch=channel(); send(ch,\"hello\"); let x=recv(ch);").is_ok());
    }
    #[test]
    fn await_syntax_and_introspection() {
        let src = "class A {} class B: A {} fn square(n: Int)->Int{return n*n;} let task=spawn(\"square\",[6]); let result=await task; let b=B(); let t=type_of(b); let yes=is_instance(b,\"A\"); let field=has_field(b,\"x\");";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn lambda_function_values() {
        let src = "let offset=4; let add: Function=fn(x: Int)->Int{return x+offset;}; let y=add(6); assert(y==10); assert(type_of(add)==\"Function\");";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn named_function_values() {
        let src = "fn twice(x: Int)->Int{return x*2;} let f: Function=twice; let ys=map_call(f,[1,2,3]); assert(ys[0]==2); assert(ys[2]==6);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn list_comprehensions() {
        let src = "let factor=3; let xs=[x*factor for x in [1,2,3,4] if x%2==0]; assert(len(xs)==2); assert(xs[0]==6); assert(xs[1]==12);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn match_case_statement() {
        let src = "let x=2; let label=\"\"; match x { case 1 { label=\"one\"; } case 2 { label=\"two\"; } case _ { label=\"other\"; } } assert(label==\"two\");";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn advanced_linear_algebra() {
        let src = "let d=matrix_det([[4,7],[2,6]]); assert(abs(d-10)<0.000001); let inv=matrix_inverse([[4,7],[2,6]]); assert(abs(inv[0][0]-0.6)<0.000001); let x=linear_solve([[3,2],[1,2]],[5,5]); assert(abs(x[0])<0.000001); assert(abs(x[1]-2.5)<0.000001);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn generators_yield_and_collect() {
        let src = "fn count_to(n: Int) -> Generator { let i=0; while i<n { yield i; i=i+1; } } let g=count_to(4); let a=next(g); let b=next(g); let rest=collect(g); assert(a==0); assert(b==1); assert(len(rest)==2);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn lsp_navigation_helpers() {
        let src = "fn add(x: Int)->Int{return x+1;} let y=add(4); let z=add(y);";
        let def = lsp_definition(src, "file:///test.vy", "add");
        assert!(!def.is_null());
        let refs = lsp_references(src, "file:///test.vy", "add");
        assert!(refs.as_array().map(|x| x.len()).unwrap_or(0) >= 3);
    }
    #[test]
    fn grouped_data_and_plotting() {
        let scatter = env::temp_dir().join("veyra_scatter.svg");
        let bars = env::temp_dir().join("veyra_bars.svg");
        let src = format!(
            "let rows=[{{\"g\":\"a\",\"x\":1}},{{\"g\":\"a\",\"x\":3}},{{\"g\":\"b\",\"x\":4}}]; let sums=table_group_sum(rows,\"g\",\"x\"); let means=table_group_mean(rows,\"g\",\"x\"); let h=histogram([1,1,2,3,4,4],3); scatter_svg(\"{}\",[1,2,3],[3,2,5]); bar_svg(\"{}\",[\"a\",\"b\"],[4,5]);",
            scatter.to_string_lossy().replace('\\', "\\\\"),
            bars.to_string_lossy().replace('\\', "\\\\")
        );
        assert!(run_source(&src).is_ok());
        assert!(scatter.exists());
        assert!(bars.exists());
        let _ = fs::remove_file(scatter);
        let _ = fs::remove_file(bars);
    }
    #[test]
    fn async_functions_methods_and_gather() {
        let src = "async fn square(n: Int)->Int{sleep_ms(1);return n*n;} class Worker { async fn plus(self,n:Int)->Int{return n+1;} } let a=square(3); let b=square(4); let values=gather([a,b]); assert(values[0]==9); assert(values[1]==16); let w=Worker(); let x=await w.plus(7); assert(x==8);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn context_manager_with_as() {
        let src = "class Resource { fn init(self)->Null{self.closed=false;return null;} fn enter(self)->Resource{return self;} fn exit(self)->Null{self.closed=true;return null;} } with Resource() as r { assert(r.closed==false); } assert(r.closed==true);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn decorator_syntax_identity() {
        let src = "fn identity(f: Function)->Function{return f;} @identity fn plus_one(x:Int)->Int{return x+1;} assert(plus_one(4)==5);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn escaping_closure_objects() {
        let src = "fn twice_result(f:Function)->Function{return fn(x:Int)->Int{return f(x)*2;};} @twice_result fn plus_one(x:Int)->Int{return x+1;} fn make_adder(offset:Int)->Function{return fn(x:Int)->Int{return x+offset;};} let add7=make_adder(7); assert(plus_one(4)==10); assert(add7(5)==12);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn module_map_dispatch() {
        let mut vm = Vm::new(compile_source("fn plus_one(x:Int)->Int{return x+1;}").unwrap());
        vm.run().unwrap();
        let def = vm.funcs.get("plus_one").unwrap().clone();
        let key = export_function_object(def).unwrap();
        let module = Val::Map(vec![(Val::Str("plus".into()), Val::Function(key))]);
        let mut caller = Vm::new(vec![
            Op::Load("module".into()),
            Op::Const(Val::Num(4.0)),
            Op::CallMethod("plus".into(), 1),
            Op::Const(Val::Num(5.0)),
            Op::Eq,
            Op::Call("assert".into(), 1),
            Op::Halt,
        ]);
        caller.vars.insert("module".into(), module);
        assert!(caller.run().is_ok());
    }
    #[test]
    fn ml_metrics_and_split() {
        let src = "let m=classification_metrics([1,0,1,1],[1,0,0,1]); assert(m[\"accuracy\"]==0.75); assert(m[\"precision\"]==1); let s=train_test_split([1,2,3,4],[10,20,30,40],0.25); assert(len(s[\"train_x\"])==3); assert(len(s[\"test_x\"])==1);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn path_and_text_services() {
        let src = "assert(starts_with(\"veyra\",\"vey\")); assert(ends_with(\"veyra\",\"yra\")); assert(replace_text(\"a-b\",\"-\",\"+\")==\"a+b\"); let p=path_join(\"a\",\"b.txt\"); assert(path_name(p)==\"b.txt\"); assert(path_extension(p)==\"txt\");";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn finally_cleanup_semantics() {
        let src = "let cleaned=false; try { throw \"boom\"; } catch err { assert(err==\"boom\"); } finally { cleaned=true; } assert(cleaned); let normal=false; try { normal=true; } catch err {} finally { normal=true; } assert(normal);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn dataframe_extended_helpers() {
        let src = "let rows=[{\"id\":1,\"x\":1},{\"id\":2,\"x\":null},{\"id\":3,\"x\":3}]; let h=table_head(rows,2); let t=table_tail(rows,1); assert(len(h)==2); assert(t[0][\"id\"]==3); let d=table_drop_nulls(rows,\"x\"); assert(len(d)==2); let f=table_fill_null(rows,\"x\",9); assert(f[1][\"x\"]==9); let joined=table_left_join(rows,[{\"rid\":1,\"name\":\"a\"}],\"id\",\"rid\"); assert(len(joined)==3);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn tensor_autograd_and_training() {
        let src = "let x=tensor([[1],[2],[3],[4]],false); let y=tensor([[2],[4],[6],[8]],false); let w=tensor([[0]],true); let epoch=0; while epoch<40 { let pred=tensor_matmul(x,w); let diff=tensor_sub(pred,y); let sq=tensor_mul(diff,diff); let loss=tensor_mean(sq); tensor_backward(loss); tensor_step(w,0.05); tensor_zero_grad(w); epoch=epoch+1; } let trained=tensor_values(w); assert(trained[0]>1.9 and trained[0]<2.1);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn loop_local_declaration_reinitializes() {
        let src = "let i=0; let total=0; while i<3 { let local=i+1; total=total+local; i=i+1; } assert(total==6);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn tensor_extended_autograd_and_shapes() {
        let src = "let x=tensor([1,2],true); let y=tensor_sigmoid(x); let loss=tensor_sum(y); tensor_backward(loss); let g=tensor_grad(x); assert(g[0]>0.19 and g[0]<0.20); assert(g[1]>0.10 and g[1]<0.11); let m=tensor([[1,2,3],[4,5,6]],false); let tr=tensor_transpose(m); let sh=tensor_shape(tr); assert(sh[0]==3 and sh[1]==2); let r=tensor_reshape(tr,[6]); let v=tensor_values(r); assert(v[0]==1 and v[1]==4 and v[5]==6);";
        if let Err(e) = run_source(src) {
            panic!("extended tensor test failed: {e}");
        }
    }
    #[test]
    fn tensor_advanced_ops_and_softmax() {
        let src = "let x=tensor([2,4],true); let y=tensor_pow(x,2); let loss=tensor_sum(y); tensor_backward(loss); let g=tensor_grad(x); assert(g[0]==4 and g[1]==8); let a=tensor([6,8],true); let b=tensor([2,4],false); let q=tensor_div(a,b); let qloss=tensor_sum(q); tensor_backward(qloss); let qg=tensor_grad(a); assert(qg[0]==0.5 and qg[1]==0.25); let n=tensor_neg(a); let nv=tensor_values(n); assert(nv[0]==-6 and nv[1]==-8); let s=tensor_softmax(tensor([[1,2,3],[2,2,2]],false)); let sv=tensor_values(s); assert(sv[2]>sv[1] and sv[1]>sv[0]); assert(sv[3]>0.33 and sv[3]<0.34); assert(tensor_numel(s)==6);";
        if let Err(e) = run_source(src) {
            panic!("advanced tensor ops failed: {e}");
        }
    }
    #[test]
    fn adam_optimizer_trains_parameter() {
        let src = "let target=tensor([3],false); let w=tensor([0],true); let epoch=0; while epoch<120 { let diff=tensor_sub(w,target); let loss=tensor_mean(tensor_pow(diff,2)); tensor_backward(loss); tensor_adam_step(w,0.1,0.9,0.999,0.00000001); tensor_zero_grad(w); epoch=epoch+1; } let value=tensor_values(w); assert(value[0]>2.9 and value[0]<3.1); tensor_reset_optimizer(w);";
        if let Err(e) = run_source(src) {
            panic!("adam optimizer test failed: {e}");
        }
    }
    #[test]
    fn tensor_serialization_round_trip() {
        let path = env::temp_dir().join("veyra_tensor_round_trip.json");
        let escaped = path.to_string_lossy().replace('\\', "\\\\");
        let src = format!(
            "let x=tensor([[1,2],[3,4]],true); tensor_save(x,\"{escaped}\"); let y=tensor_load(\"{escaped}\"); let sh=tensor_shape(y); let v=tensor_values(y); assert(sh[0]==2 and sh[1]==2); assert(v[0]==1 and v[3]==4);"
        );
        assert!(run_source(&src).is_ok());
        let _ = fs::remove_file(path);
    }
    #[test]
    fn task_timeout_and_status() {
        let src = "fn slow()->Int{sleep_ms(25);return 7;} let t=spawn(slow,[]); let s=task_status(t); assert(s==\"running\" or s==\"finished\"); let early=await_timeout(t,1); assert(early==null); let value=await_task(t); assert(value==7);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn lsp_rename_helper() {
        let src = "fn add(x:Int)->Int{return x+1;} let a=add(1); let b=add(a);";
        let edit = lsp_rename(src, "file:///test.vy", "add", "increment");
        let changes = edit
            .get("changes")
            .and_then(|x| x.get("file:///test.vy"))
            .and_then(JsonValue::as_array)
            .map(|x| x.len())
            .unwrap_or(0);
        assert!(changes >= 3);
    }
    #[test]
    fn security_primitives() {
        let src = "let mac=hmac_sha256(\"key\",\"message\"); assert(len(mac)==64); let derived=pbkdf2_sha256(\"password\",\"salt\",100,32); assert(len(derived)==64); assert(constant_time_equal(mac,mac)); assert(not constant_time_equal(mac,derived));";
        assert!(run_source(src).is_ok());
    }
    #[cfg(feature = "native-services")]
    #[test]
    fn sqlite_parameter_binding() {
        let db = env::temp_dir().join("veyra_params_test.sqlite");
        let escaped = db.to_string_lossy().replace('\\', "\\\\");
        let src = format!(
            "sqlite_exec(\"{escaped}\",\"CREATE TABLE IF NOT EXISTS items(id INTEGER,name TEXT)\"); sqlite_exec(\"{escaped}\",\"DELETE FROM items\"); sqlite_exec_params(\"{escaped}\",\"INSERT INTO items(id,name) VALUES (?,?)\",[7,\"veyra\"]); let rows=sqlite_query_params(\"{escaped}\",\"SELECT id,name FROM items WHERE id=?\",[7]); assert(len(rows)==1); assert(rows[0][\"name\"]==\"veyra\");"
        );
        assert!(run_source(&src).is_ok());
        let _ = fs::remove_file(db);
    }
}
