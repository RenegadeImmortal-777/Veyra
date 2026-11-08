// Recursive-descent parser: `Tok` stream to `Expr`/`Stmt`.
//
// Extracted from `main.rs` without behavior change; see docs/ENGINE-SPLIT.md.

use crate::ast::{Expr, Stmt};
use crate::lexer::Tok;

pub(crate) struct Parser {
    t: Vec<Tok>,
    lines: Vec<usize>,
    i: usize,
}
impl Parser {
    pub(crate) fn new(t: Vec<Tok>) -> Self {
        let len = t.len();
        Self {
            t,
            lines: vec![0; len],
            i: 0,
        }
    }
    pub(crate) fn new_with_lines(tokens: Vec<(Tok, usize)>) -> Self {
        let (t, lines) = tokens.into_iter().unzip();
        Self { t, lines, i: 0 }
    }
    fn line(&self) -> usize {
        self.lines.get(self.i).copied().unwrap_or(0)
    }
    fn fmt_err(&self, msg: String) -> String {
        let l = self.line();
        if l > 0 {
            format!("line {l}: {msg}")
        } else {
            msg
        }
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
            Err(self.fmt_err(format!("{msg}; got {:?}", self.cur())))
        }
    }
    pub(crate) fn program(&mut self) -> Result<Vec<Stmt>, String> {
        let mut v = vec![];
        while !matches!(self.cur(), Tok::Eof) {
            let mut s = self.stmt()?;
            if let Stmt::If(Expr::Bool(true), ref mut unpacked, ref empty) = s {
                if empty.is_empty()
                    && matches!(unpacked.first(), Some(Stmt::Let(n, _, _, _)) if n.starts_with("__unpack_"))
                {
                    v.append(unpacked);
                    continue;
                }
            }
            v.push(s);
        }
        Ok(v)
    }
    fn block(&mut self) -> Result<Vec<Stmt>, String> {
        self.need(&Tok::Lb, "expected '{'")?;
        let mut v = vec![];
        while !matches!(self.cur(), Tok::Rb | Tok::Eof) {
            let mut s = self.stmt()?;
            if let Stmt::If(Expr::Bool(true), ref mut unpacked, ref empty) = s {
                if empty.is_empty()
                    && matches!(unpacked.first(), Some(Stmt::Let(n, _, _, _)) if n.starts_with("__unpack_"))
                {
                    v.append(unpacked);
                    continue;
                }
            }
            v.push(s);
        }
        self.need(&Tok::Rb, "expected '}'")?;
        Ok(v)
    }
    fn stmt(&mut self) -> Result<Stmt, String> {
        if matches!(self.cur(), Tok::Id(s) if s == "pass") {
            self.i += 1;
            self.eat(&Tok::Semi);
            return Ok(Stmt::Expr(Expr::Null));
        }
        if self.eat(&Tok::Let) || self.eat(&Tok::Const) {
            let is_const = matches!(self.t[self.i - 1], Tok::Const);
            if self.eat(&Tok::Ls) || self.eat(&Tok::Lp) {
                let closing = if matches!(self.t[self.i - 1], Tok::Ls) {
                    Tok::Rs
                } else {
                    Tok::Rp
                };
                let mut names = vec![];
                while !self.eat(&closing) {
                    match self.bump() {
                        Tok::Id(s) => names.push(s),
                        x => return Err(format!("expected destructuring name, got {x:?}")),
                    }
                    if self.eat(&closing) {
                        break;
                    }
                    self.need(&Tok::Comma, "expected ',' in destructuring")?;
                }
                self.need(&Tok::Eq, "expected '=' after destructuring pattern")?;
                let rhs = self.expr()?;
                self.eat(&Tok::Semi);
                let tmp = format!("__unpack_{}", self.i);
                let mut stmts = vec![Stmt::Let(tmp.clone(), None, rhs, false)];
                for (idx, n) in names.into_iter().enumerate() {
                    stmts.push(Stmt::Let(
                        n,
                        None,
                        Expr::Index(
                            Box::new(Expr::Var(tmp.clone())),
                            Box::new(Expr::Num(idx as f64)),
                        ),
                        is_const,
                    ));
                }
                return Ok(Stmt::If(Expr::Bool(true), stmts, vec![]));
            }
            let name = match self.bump() {
                Tok::Id(s) => s,
                x => return Err(format!("expected name, got {x:?}")),
            };
            if self.eat(&Tok::Comma) {
                let mut names = vec![name];
                loop {
                    match self.bump() {
                        Tok::Id(s) => names.push(s),
                        x => return Err(format!("expected destructuring name, got {x:?}")),
                    }
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                self.need(&Tok::Eq, "expected '='")?;
                let rhs = self.expr()?;
                self.eat(&Tok::Semi);
                let tmp = format!("__unpack_{}", self.i);
                let mut stmts = vec![Stmt::Let(tmp.clone(), None, rhs, false)];
                for (idx, n) in names.into_iter().enumerate() {
                    stmts.push(Stmt::Let(
                        n,
                        None,
                        Expr::Index(
                            Box::new(Expr::Var(tmp.clone())),
                            Box::new(Expr::Num(idx as f64)),
                        ),
                        is_const,
                    ));
                }
                return Ok(Stmt::If(Expr::Bool(true), stmts, vec![]));
            }
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
                if matches!(self.cur(), Tok::If) {
                    vec![self.stmt()?]
                } else {
                    self.block()?
                }
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
            if self.eat(&Tok::Comma) {
                let mut names = vec![name];
                loop {
                    match self.bump() {
                        Tok::Id(s) => names.push(s),
                        x => return Err(format!("expected loop variable, got {x:?}")),
                    }
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                self.need(&Tok::In, "expected 'in'")?;
                let iterable = self.expr()?;
                let mut body = self.block()?;
                let tmp = format!("__for_item_{}", self.i);
                let mut prefix = Vec::with_capacity(names.len());
                for (idx, n) in names.into_iter().enumerate() {
                    prefix.push(Stmt::Let(
                        n,
                        None,
                        Expr::Index(
                            Box::new(Expr::Var(tmp.clone())),
                            Box::new(Expr::Num(idx as f64)),
                        ),
                        false,
                    ));
                }
                prefix.append(&mut body);
                return Ok(Stmt::ForEach(tmp, iterable, prefix));
            }
            self.need(&Tok::In, "expected 'in'")?;
            if matches!(self.cur(), Tok::Id(s) if s == "range")
                && self.i + 1 < self.t.len()
                && matches!(self.t[self.i + 1], Tok::Lp)
            {
                self.i += 2;
                let first = self.expr()?;
                if self.eat(&Tok::Rp) {
                    let body = self.block()?;
                    return Ok(Stmt::ForRange(name, Expr::Num(0.0), first, body));
                }
                self.need(&Tok::Comma, "expected ',' in range")?;
                let second = self.expr()?;
                if self.eat(&Tok::Rp) {
                    let body = self.block()?;
                    return Ok(Stmt::ForRange(name, first, second, body));
                }
                self.need(&Tok::Comma, "expected ',' in range")?;
                let step = self.expr()?;
                self.need(&Tok::Rp, "expected ')' after range")?;
                let body = self.block()?;
                return Ok(Stmt::ForEach(
                    name,
                    Expr::Call("range".into(), vec![first, second, step]),
                    body,
                ));
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
        let e = self.expr()?;
        let assign_op = match self.cur() {
            Tok::Eq => Some(None),
            Tok::PlusEq => Some(Some("+")),
            Tok::MinusEq => Some(Some("-")),
            Tok::StarEq => Some(Some("*")),
            Tok::SlashEq => Some(Some("/")),
            _ => None,
        };
        if let Some(compound_op) = assign_op {
            self.i += 1;
            let rhs = self.expr()?;
            self.eat(&Tok::Semi);
            let value = match compound_op {
                Some(op) => Expr::Bin(Box::new(e.clone()), op.into(), Box::new(rhs)),
                None => rhs,
            };
            return match e {
                Expr::Var(n) => Ok(Stmt::Assign(n, value)),
                Expr::Member(obj, name) => Ok(Stmt::SetMember(*obj, name, value)),
                Expr::Index(target, idx) => Ok(Stmt::SetIndex(*target, *idx, value)),
                _ => Err("invalid assignment target".into()),
            };
        }
        self.eat(&Tok::Semi);
        if let Expr::MethodCall(ref recv, ref mname, _) = e {
            if let Expr::Var(var_name) = &**recv {
                if matches!(
                    mname.as_str(),
                    "append"
                        | "push"
                        | "extend"
                        | "sort"
                        | "reverse"
                        | "clear"
                        | "insert"
                        | "remove"
                ) {
                    return Ok(Stmt::Assign(var_name.clone(), e));
                }
            }
        }
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
            if matches!(self.cur(), Tok::Id(s) if s == "is") {
                self.i += 1;
                let op = if self.eat(&Tok::Not) { "!=" } else { "==" }.to_string();
                e = Expr::Bin(Box::new(e), op, Box::new(self.compare()?));
                continue;
            }
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
            if self.eat(&Tok::In) {
                let rhs = self.term()?;
                e = Expr::Call("contains".into(), vec![rhs, e]);
                continue;
            }
            if matches!(self.cur(), Tok::Not)
                && self.i + 1 < self.t.len()
                && matches!(self.t[self.i + 1], Tok::In)
            {
                self.i += 2;
                let rhs = self.term()?;
                e = Expr::Unary(
                    "!".into(),
                    Box::new(Expr::Call("contains".into(), vec![rhs, e])),
                );
                continue;
            }
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
        self.power()
    }
    fn power(&mut self) -> Result<Expr, String> {
        let base = self.postfix()?;
        if self.eat(&Tok::StarStar) {
            let exp = self.unary()?;
            Ok(Expr::Bin(Box::new(base), "**".into(), Box::new(exp)))
        } else {
            Ok(base)
        }
    }
    fn postfix(&mut self) -> Result<Expr, String> {
        let mut e = self.primary()?;
        loop {
            if self.eat(&Tok::Ls) {
                let first = if matches!(self.cur(), Tok::Colon) {
                    Expr::Null
                } else {
                    self.expr()?
                };
                if self.eat(&Tok::Colon) {
                    let stop = if matches!(self.cur(), Tok::Colon | Tok::Rs) {
                        Expr::Null
                    } else {
                        self.expr()?
                    };
                    let step = if self.eat(&Tok::Colon) {
                        if matches!(self.cur(), Tok::Rs) {
                            Expr::Null
                        } else {
                            self.expr()?
                        }
                    } else {
                        Expr::Null
                    };
                    self.need(&Tok::Rs, "expected ']' after slice")?;
                    e = Expr::Call("py_slice".into(), vec![e, first, stop, step]);
                } else {
                    self.need(&Tok::Rs, "expected ']' after index")?;
                    e = Expr::Index(Box::new(e), Box::new(first));
                }
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
                if n == "lambda" && !matches!(self.cur(), Tok::Lp) {
                    let mut params = vec![];
                    if !self.eat(&Tok::Colon) {
                        loop {
                            let param = match self.bump() {
                                Tok::Id(s) => s,
                                x => return Err(format!("expected lambda parameter, got {x:?}")),
                            };
                            params.push((param, None));
                            if self.eat(&Tok::Colon) {
                                break;
                            }
                            self.need(&Tok::Comma, "expected ',' or ':' in lambda")?;
                        }
                    }
                    let body_expr = self.expr()?;
                    return Ok(Expr::Lambda(
                        params,
                        None,
                        vec![Stmt::Return(Some(body_expr))],
                    ));
                }
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
                if self.eat(&Tok::Rp) {
                    return Ok(Expr::List(vec![]));
                }
                let first = self.expr()?;
                if self.eat(&Tok::Comma) {
                    let mut items = vec![first];
                    if !self.eat(&Tok::Rp) {
                        loop {
                            items.push(self.expr()?);
                            if self.eat(&Tok::Rp) {
                                break;
                            }
                            self.need(&Tok::Comma, "expected ',' in tuple")?;
                            if self.eat(&Tok::Rp) {
                                break;
                            }
                        }
                    }
                    return Ok(Expr::Call("tuple".into(), vec![Expr::List(items)]));
                }
                self.need(&Tok::Rp, "expected ')'")?;
                Ok(first)
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
                if self.eat(&Tok::Rb) {
                    return Ok(Expr::Map(vec![]));
                }
                let first = self.expr()?;
                if self.eat(&Tok::Colon) {
                    let first_val = self.expr()?;
                    let mut pairs = vec![(first, first_val)];
                    if !self.eat(&Tok::Rb) {
                        self.need(&Tok::Comma, "expected ',' in map")?;
                        while !self.eat(&Tok::Rb) {
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
                } else {
                    let mut items = vec![first];
                    if !self.eat(&Tok::Rb) {
                        self.need(&Tok::Comma, "expected ',' in set")?;
                        while !self.eat(&Tok::Rb) {
                            items.push(self.expr()?);
                            if self.eat(&Tok::Rb) {
                                break;
                            }
                            self.need(&Tok::Comma, "expected ',' in set")?;
                        }
                    }
                    Ok(Expr::Call("set".into(), vec![Expr::List(items)]))
                }
            }
            x => Err(self.fmt_err(format!("expected expression, got {x:?}"))),
        }
    }
}
