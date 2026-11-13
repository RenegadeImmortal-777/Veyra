// Bytecode compiler: AST to `Op` stream.
//
// Extracted from `main.rs` without behavior change; see docs/ENGINE-SPLIT.md.

use std::collections::{HashMap, HashSet};

use crate::ast::{CallArg, Expr, Stmt};
use crate::bytecode::Op;
use crate::value::Val;

#[derive(Clone, Debug, Default)]
pub(crate) struct CompiledDebug {
    pub(crate) lines: Vec<(usize, usize)>,
    pub(crate) children: Vec<(usize, usize, CompiledDebug)>,
    pub(crate) local_names: Vec<String>,
}

fn collect_all_ids_expr(e: &Expr, out: &mut HashSet<String>) {
    match e {
        Expr::Var(n) => {
            out.insert(n.clone());
        }
        Expr::Call(n, args) => {
            out.insert(n.clone());
            for a in args {
                collect_all_ids_expr(a, out);
            }
        }
        Expr::CallExpr(callee, args) => {
            collect_all_ids_expr(callee, out);
            for a in args {
                collect_all_ids_expr(a, out);
            }
        }
        Expr::CallExt {
            callee,
            method,
            args,
        } => {
            if method.is_none() {
                if let Expr::Var(n) = &**callee {
                    out.insert(n.clone());
                } else {
                    collect_all_ids_expr(callee, out);
                }
            } else {
                collect_all_ids_expr(callee, out);
            }
            for a in args {
                match a {
                    CallArg::Pos(e) | CallArg::Star(e) | CallArg::Kw(_, e) | CallArg::KwStar(e) => {
                        collect_all_ids_expr(e, out)
                    }
                }
            }
        }
        Expr::ChainedCmp(first, links) => {
            collect_all_ids_expr(first, out);
            for (_, rhs) in links {
                collect_all_ids_expr(rhs, out);
            }
        }
        Expr::List(items) => {
            for x in items {
                collect_all_ids_expr(x, out);
            }
        }
        Expr::Map(pairs) => {
            for (k, v) in pairs {
                collect_all_ids_expr(k, out);
                collect_all_ids_expr(v, out);
            }
        }
        Expr::Index(a, b) | Expr::Bin(a, _, b) => {
            collect_all_ids_expr(a, out);
            collect_all_ids_expr(b, out);
        }
        Expr::Member(a, _) | Expr::Await(a) | Expr::Unary(_, a) => collect_all_ids_expr(a, out),
        Expr::MethodCall(a, _, args) => {
            collect_all_ids_expr(a, out);
            for x in args {
                collect_all_ids_expr(x, out);
            }
        }
        Expr::Ternary(a, b, c) => {
            collect_all_ids_expr(a, out);
            collect_all_ids_expr(b, out);
            collect_all_ids_expr(c, out);
        }
        Expr::ListComp(val, _, iter, cond) => {
            collect_all_ids_expr(val, out);
            collect_all_ids_expr(iter, out);
            if let Some(c) = cond {
                collect_all_ids_expr(c, out);
            }
        }
        Expr::Lambda(_, _, body) => collect_all_ids_stmts(body, out),
        Expr::Num(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Null => {}
    }
}

fn collect_all_ids_stmts(stmts: &[Stmt], out: &mut HashSet<String>) {
    for s in stmts {
        match s {
            Stmt::Let(_, _, e, _) | Stmt::Expr(e) | Stmt::Throw(e) | Stmt::Yield(e) => {
                collect_all_ids_expr(e, out);
            }
            Stmt::Assign(n, e) => {
                out.insert(n.clone());
                collect_all_ids_expr(e, out);
            }
            Stmt::If(c, a, b) => {
                collect_all_ids_expr(c, out);
                collect_all_ids_stmts(a, out);
                collect_all_ids_stmts(b, out);
            }
            Stmt::While(c, body) => {
                collect_all_ids_expr(c, out);
                collect_all_ids_stmts(body, out);
            }
            Stmt::ForRange(n, a, b, body) => {
                out.insert(n.clone());
                collect_all_ids_expr(a, out);
                collect_all_ids_expr(b, out);
                collect_all_ids_stmts(body, out);
            }
            Stmt::ForEach(n, e, body) | Stmt::With(e, n, body) => {
                out.insert(n.clone());
                collect_all_ids_expr(e, out);
                collect_all_ids_stmts(body, out);
            }
            Stmt::Try(body, _, handler, finally) => {
                collect_all_ids_stmts(body, out);
                collect_all_ids_stmts(handler, out);
                collect_all_ids_stmts(finally, out);
            }
            Stmt::Match(e, cases) => {
                collect_all_ids_expr(e, out);
                for (p, body) in cases {
                    if let Some(pe) = p {
                        collect_all_ids_expr(pe, out);
                    }
                    collect_all_ids_stmts(body, out);
                }
            }
            Stmt::Fn(_, _, _, body)
            | Stmt::AsyncFn(_, _, _, body)
            | Stmt::DecoratedFn(_, _, _, _, body)
            | Stmt::Class(_, _, body) => {
                collect_all_ids_stmts(body, out);
            }
            Stmt::SetMember(a, _, b) => {
                collect_all_ids_expr(a, out);
                collect_all_ids_expr(b, out);
            }
            Stmt::SetIndex(t, i, v) => {
                collect_all_ids_expr(t, out);
                collect_all_ids_expr(i, out);
                collect_all_ids_expr(v, out);
            }
            Stmt::Return(Some(e)) => collect_all_ids_expr(e, out),
            Stmt::Return(None) | Stmt::Break | Stmt::Continue | Stmt::SourceLine(_) => {}
        }
    }
}

/// Collects local names that cannot live in unboxed fast slots because they are
/// referenced inside nested closures/classes or invoked as dynamic `callee(...)` locals.
fn collect_unslotted_in_expr(e: &Expr, out: &mut HashSet<String>) {
    match e {
        Expr::Call(name, args) => {
            out.insert(name.clone());
            for a in args {
                collect_unslotted_in_expr(a, out);
            }
        }
        Expr::CallExpr(callee, args) => {
            collect_unslotted_in_expr(callee, out);
            for a in args {
                collect_unslotted_in_expr(a, out);
            }
        }
        Expr::CallExt {
            callee,
            method,
            args,
        } => {
            if method.is_none() {
                if let Expr::Var(name) = &**callee {
                    out.insert(name.clone());
                } else {
                    collect_unslotted_in_expr(callee, out);
                }
            } else {
                collect_unslotted_in_expr(callee, out);
            }
            for a in args {
                match a {
                    CallArg::Pos(e) | CallArg::Star(e) | CallArg::Kw(_, e) | CallArg::KwStar(e) => {
                        collect_unslotted_in_expr(e, out)
                    }
                }
            }
        }
        Expr::ChainedCmp(first, links) => {
            collect_unslotted_in_expr(first, out);
            for (_, rhs) in links {
                collect_unslotted_in_expr(rhs, out);
            }
        }
        Expr::Lambda(_, _, body) => collect_all_ids_stmts(body, out),
        Expr::ListComp(val, _, iter, cond) => {
            collect_all_ids_expr(val, out);
            collect_unslotted_in_expr(iter, out);
            if let Some(c) = cond {
                collect_all_ids_expr(c, out);
            }
        }
        Expr::List(items) => {
            for x in items {
                collect_unslotted_in_expr(x, out);
            }
        }
        Expr::Map(pairs) => {
            for (k, v) in pairs {
                collect_unslotted_in_expr(k, out);
                collect_unslotted_in_expr(v, out);
            }
        }
        Expr::Index(a, b) | Expr::Bin(a, _, b) => {
            collect_unslotted_in_expr(a, out);
            collect_unslotted_in_expr(b, out);
        }
        Expr::Member(a, _) | Expr::Await(a) | Expr::Unary(_, a) => {
            collect_unslotted_in_expr(a, out);
        }
        Expr::MethodCall(a, _, args) => {
            collect_unslotted_in_expr(a, out);
            for x in args {
                collect_unslotted_in_expr(x, out);
            }
        }
        Expr::Ternary(a, b, c) => {
            collect_unslotted_in_expr(a, out);
            collect_unslotted_in_expr(b, out);
            collect_unslotted_in_expr(c, out);
        }
        Expr::Var(_) | Expr::Num(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Null => {}
    }
}

fn collect_unslotted_in_stmts(stmts: &[Stmt], out: &mut HashSet<String>) {
    for s in stmts {
        match s {
            Stmt::Fn(_, _, _, body) | Stmt::AsyncFn(_, _, _, body) => {
                collect_all_ids_stmts(body, out);
            }
            Stmt::DecoratedFn(_, name, _, _, body) => {
                out.insert(name.clone());
                collect_all_ids_stmts(body, out);
            }
            Stmt::Class(_, _, body) => collect_all_ids_stmts(body, out),
            Stmt::Try(body, err_name, handler, finally) => {
                out.insert(err_name.clone());
                collect_unslotted_in_stmts(body, out);
                collect_unslotted_in_stmts(handler, out);
                collect_unslotted_in_stmts(finally, out);
            }
            Stmt::Let(_, _, e, _)
            | Stmt::Assign(_, e)
            | Stmt::Expr(e)
            | Stmt::Throw(e)
            | Stmt::Yield(e) => collect_unslotted_in_expr(e, out),
            Stmt::If(c, a, b) => {
                collect_unslotted_in_expr(c, out);
                collect_unslotted_in_stmts(a, out);
                collect_unslotted_in_stmts(b, out);
            }
            Stmt::While(c, body) => {
                collect_unslotted_in_expr(c, out);
                collect_unslotted_in_stmts(body, out);
            }
            Stmt::ForRange(_, a, b, body) => {
                collect_unslotted_in_expr(a, out);
                collect_unslotted_in_expr(b, out);
                collect_unslotted_in_stmts(body, out);
            }
            Stmt::ForEach(_, e, body) | Stmt::With(e, _, body) => {
                collect_unslotted_in_expr(e, out);
                collect_unslotted_in_stmts(body, out);
            }
            Stmt::Match(e, cases) => {
                collect_unslotted_in_expr(e, out);
                for (p, body) in cases {
                    if let Some(pe) = p {
                        collect_unslotted_in_expr(pe, out);
                    }
                    collect_unslotted_in_stmts(body, out);
                }
            }
            Stmt::SetMember(a, _, b) => {
                collect_unslotted_in_expr(a, out);
                collect_unslotted_in_expr(b, out);
            }
            Stmt::SetIndex(t, i, v) => {
                collect_unslotted_in_expr(t, out);
                collect_unslotted_in_expr(i, out);
                collect_unslotted_in_expr(v, out);
            }
            Stmt::Return(Some(e)) => collect_unslotted_in_expr(e, out),
            Stmt::Return(None) | Stmt::Break | Stmt::Continue | Stmt::SourceLine(_) => {}
        }
    }
}

struct LoopCtx {
    breaks: Vec<usize>,
    continues: Vec<usize>,
}

pub(crate) struct Compiler {
    code: Vec<Op>,
    next_temp: usize,
    loops: Vec<LoopCtx>,
    cur_line: usize,
    line_marks: Vec<(usize, usize)>,
    child_debugs: Vec<(usize, usize, CompiledDebug)>,
    local_mode: bool,
    local_slots: HashMap<String, usize>,
    slot_names: Vec<String>,
    unslotted: HashSet<String>,
}
impl Compiler {
    pub(crate) fn new() -> Self {
        Self::new_at_line(0)
    }
    fn new_at_line(cur_line: usize) -> Self {
        Self {
            code: vec![],
            next_temp: 0,
            loops: vec![],
            cur_line,
            line_marks: vec![],
            child_debugs: vec![],
            local_mode: false,
            local_slots: HashMap::new(),
            slot_names: vec![],
            unslotted: HashSet::new(),
        }
    }
    fn enable_local_scope(
        &mut self,
        params: &[(String, Option<String>)],
        body: &[Stmt],
        extra_exprs: &[&Expr],
    ) {
        self.local_mode = true;
        collect_unslotted_in_stmts(body, &mut self.unslotted);
        for e in extra_exprs {
            collect_unslotted_in_expr(e, &mut self.unslotted);
        }
        for (slot, (param, _)) in params.iter().enumerate() {
            self.slot_names.push(param.clone());
            if !self.unslotted.contains(param) {
                self.local_slots.insert(param.clone(), slot);
            }
        }
    }
    fn alloc_slot(&mut self, name: &str) -> Option<usize> {
        if !self.local_mode || self.unslotted.contains(name) {
            return None;
        }
        if let Some(&slot) = self.local_slots.get(name) {
            return Some(slot);
        }
        let slot = self.slot_names.len();
        self.slot_names.push(name.to_string());
        self.local_slots.insert(name.to_string(), slot);
        Some(slot)
    }
    fn emit_load(&mut self, name: &str) {
        if let Some(&slot) = self.local_slots.get(name) {
            self.code.push(Op::LoadLocal(slot));
        } else {
            self.code.push(Op::Load(name.to_string()));
        }
    }
    fn emit_store(&mut self, name: &str) {
        if let Some(&slot) = self.local_slots.get(name) {
            self.code.push(Op::StoreLocal(slot));
        } else {
            self.code.push(Op::Store(name.to_string()));
        }
    }
    fn emit_define(&mut self, name: &str, is_const: bool, ty: Option<String>) {
        if let Some(slot) = self.alloc_slot(name) {
            self.code.push(Op::DefineLocal(slot, is_const, ty));
        } else {
            self.code.push(Op::Define(name.to_string(), is_const, ty));
        }
    }
    fn push_line_mark(&mut self, line: usize) {
        if line == 0 {
            return;
        }
        let ip = self.code.len();
        if let Some((last_ip, last_line)) = self.line_marks.last_mut() {
            if *last_ip == ip {
                *last_line = line;
                return;
            }
        }
        self.line_marks.push((ip, line));
    }
    fn compile_child(
        &self,
        params: &[(String, Option<String>)],
        body: &[Stmt],
        initial_line: usize,
    ) -> Result<(Vec<Op>, CompiledDebug), String> {
        let mut child = Compiler::new_at_line(initial_line);
        child.enable_local_scope(params, body, &[]);
        child.emit_stmts(body)?;
        if child.line_marks.is_empty() && initial_line > 0 {
            child.line_marks.push((0, initial_line));
        }
        child.code.push(Op::Const(Val::Null));
        child.code.push(Op::Return);
        let dbg = CompiledDebug {
            lines: child.line_marks,
            children: child.child_debugs,
            local_names: child.slot_names,
        };
        Ok((child.code, dbg))
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
                let (child_code, child_dbg) = self.compile_child(params, body, self.cur_line)?;
                let def_ip = self.code.len();
                self.child_debugs.push((def_ip, 0, child_dbg));
                self.code.push(Op::Def(
                    name.clone(),
                    params.clone(),
                    return_type.clone(),
                    false,
                    child_code,
                ));
                self.code.push(Op::FunctionRef(name));
            }
            Expr::ListComp(value, var, iterable, condition) => {
                let value_fn = self.temp("comp_value");
                let comp_params = vec![(var.clone(), None)];
                let mut value_compiler = Compiler::new_at_line(self.cur_line);
                if self.cur_line > 0 {
                    value_compiler.line_marks.push((0, self.cur_line));
                }
                value_compiler.enable_local_scope(&comp_params, &[], &[value]);
                value_compiler.emit_expr(value)?;
                value_compiler.code.push(Op::Return);
                let val_ip = self.code.len();
                self.child_debugs.push((
                    val_ip,
                    0,
                    CompiledDebug {
                        lines: value_compiler.line_marks,
                        children: value_compiler.child_debugs,
                        local_names: value_compiler.slot_names,
                    },
                ));
                self.code.push(Op::Def(
                    value_fn.clone(),
                    comp_params.clone(),
                    None,
                    false,
                    value_compiler.code,
                ));
                let condition_fn = if let Some(cond) = condition {
                    let name = self.temp("comp_cond");
                    let mut cond_compiler = Compiler::new_at_line(self.cur_line);
                    if self.cur_line > 0 {
                        cond_compiler.line_marks.push((0, self.cur_line));
                    }
                    cond_compiler.enable_local_scope(&comp_params, &[], &[cond]);
                    cond_compiler.emit_expr(cond)?;
                    cond_compiler.code.push(Op::Return);
                    let cond_ip = self.code.len();
                    self.child_debugs.push((
                        cond_ip,
                        0,
                        CompiledDebug {
                            lines: cond_compiler.line_marks,
                            children: cond_compiler.child_debugs,
                            local_names: cond_compiler.slot_names,
                        },
                    ));
                    self.code.push(Op::Def(
                        name.clone(),
                        comp_params,
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
                self.emit_define(&result_name, false, None);
                self.emit_expr(iterable)?;
                self.code.push(Op::Call("__iter_list".into(), 1));
                self.emit_define(&iter_name, false, None);
                self.code.push(Op::Const(Val::Num(0.0)));
                self.emit_define(&idx_name, false, Some("Int".into()));
                let loop_start = self.code.len();
                self.emit_load(&idx_name);
                self.emit_load(&iter_name);
                self.code.push(Op::Call("len".into(), 1));
                self.code.push(Op::Lt);
                let end_jump = self.code.len();
                self.code.push(Op::JumpIfFalse(0));
                let mut condition_jump = None;
                if let Some(cond_fn) = condition_fn {
                    self.emit_load(&iter_name);
                    self.emit_load(&idx_name);
                    self.code.push(Op::Index);
                    self.code.push(Op::Call(cond_fn, 1));
                    let j = self.code.len();
                    self.code.push(Op::JumpIfFalse(0));
                    condition_jump = Some(j);
                }
                self.emit_load(&result_name);
                self.emit_load(&iter_name);
                self.emit_load(&idx_name);
                self.code.push(Op::Index);
                self.code.push(Op::Call(value_fn, 1));
                self.code.push(Op::Call("append".into(), 2));
                self.emit_store(&result_name);
                let increment = self.code.len();
                if let Some(j) = condition_jump {
                    self.code[j] = Op::JumpIfFalse(increment);
                }
                self.emit_load(&idx_name);
                self.code.push(Op::Const(Val::Num(1.0)));
                self.code.push(Op::Add);
                self.emit_store(&idx_name);
                self.code.push(Op::Jump(loop_start));
                let end = self.code.len();
                self.code[end_jump] = Op::JumpIfFalse(end);
                self.emit_load(&result_name);
            }
            Expr::Var(n) => self.emit_load(n),
            Expr::Ternary(cond, then_e, else_e) => {
                self.emit_expr(cond)?;
                let jf = self.code.len();
                self.code.push(Op::JumpIfFalse(0));
                self.emit_expr(then_e)?;
                let j_end = self.code.len();
                self.code.push(Op::Jump(0));
                let else_pc = self.code.len();
                self.emit_expr(else_e)?;
                let end_pc = self.code.len();
                self.code[jf] = Op::JumpIfFalse(else_pc);
                self.code[j_end] = Op::Jump(end_pc);
            }
            Expr::Unary(op, x) => {
                self.emit_expr(x)?;
                self.code.push(if op == "-" { Op::Neg } else { Op::Not });
            }
            Expr::Bin(a, op, b) => {
                if op == "and" {
                    self.emit_expr(a)?;
                    let jf = self.code.len();
                    self.code.push(Op::JumpIfFalse(0));
                    self.emit_expr(b)?;
                    self.code.push(Op::Not);
                    self.code.push(Op::Not);
                    let j_end = self.code.len();
                    self.code.push(Op::Jump(0));
                    let false_pc = self.code.len();
                    self.code.push(Op::Const(Val::Bool(false)));
                    let end_pc = self.code.len();
                    self.code[jf] = Op::JumpIfFalse(false_pc);
                    self.code[j_end] = Op::Jump(end_pc);
                    return Ok(());
                }
                if op == "or" {
                    self.emit_expr(a)?;
                    self.code.push(Op::Not);
                    let jf = self.code.len();
                    self.code.push(Op::JumpIfFalse(0));
                    self.emit_expr(b)?;
                    self.code.push(Op::Not);
                    self.code.push(Op::Not);
                    let j_end = self.code.len();
                    self.code.push(Op::Jump(0));
                    let true_pc = self.code.len();
                    self.code.push(Op::Const(Val::Bool(true)));
                    let end_pc = self.code.len();
                    self.code[jf] = Op::JumpIfFalse(true_pc);
                    self.code[j_end] = Op::Jump(end_pc);
                    return Ok(());
                }
                self.emit_expr(a)?;
                self.emit_expr(b)?;
                self.code.push(match op.as_str() {
                    "+" => Op::Add,
                    "-" => Op::Sub,
                    "*" => Op::Mul,
                    "/" => Op::Div,
                    "%" => Op::Mod,
                    "**" => Op::Pow,
                    "==" => Op::Eq,
                    "!=" => Op::Ne,
                    "<" => Op::Lt,
                    "<=" => Op::Le,
                    ">" => Op::Gt,
                    ">=" => Op::Ge,
                    _ => return Err("bad operator".into()),
                });
            }
            Expr::ChainedCmp(first, links) => {
                let prev_name = self.temp("cmp_prev");
                let next_name = self.temp("cmp_next");
                self.emit_expr(first)?;
                self.emit_define(&prev_name, false, None);
                let mut false_jumps = Vec::new();
                for (op, rhs) in links {
                    self.emit_expr(rhs)?;
                    self.emit_define(&next_name, false, None);
                    self.emit_load(&prev_name);
                    self.emit_load(&next_name);
                    self.code.push(match op.as_str() {
                        "<" => Op::Lt,
                        "<=" => Op::Le,
                        ">" => Op::Gt,
                        ">=" => Op::Ge,
                        _ => return Err("bad comparison operator".into()),
                    });
                    let jf = self.code.len();
                    self.code.push(Op::JumpIfFalse(0));
                    false_jumps.push(jf);
                    self.emit_load(&next_name);
                    self.emit_store(&prev_name);
                }
                self.code.push(Op::Const(Val::Bool(true)));
                let end_jump = self.code.len();
                self.code.push(Op::Jump(0));
                let false_pc = self.code.len();
                self.code.push(Op::Const(Val::Bool(false)));
                let end_pc = self.code.len();
                for jf in false_jumps {
                    self.code[jf] = Op::JumpIfFalse(false_pc);
                }
                self.code[end_jump] = Op::Jump(end_pc);
            }
            Expr::Call(n, args) => {
                for a in args {
                    self.emit_expr(a)?
                }
                self.code.push(Op::Call(n.clone(), args.len()));
            }
            Expr::CallExpr(callee, args) => {
                self.emit_expr(callee)?;
                for a in args {
                    self.emit_expr(a)?;
                }
                self.code.push(Op::CallExpr(args.len()));
            }
            Expr::CallExt {
                callee,
                method,
                args,
            } => {
                let (kind, op_name) = match (method, &**callee) {
                    (None, Expr::Var(name)) => (0u8, name.clone()),
                    (Some(m), _) => {
                        self.emit_expr(callee)?;
                        (1u8, m.clone())
                    }
                    (None, _) => {
                        self.emit_expr(callee)?;
                        (2u8, String::new())
                    }
                };
                let mut specs = Vec::with_capacity(args.len());
                for a in args {
                    match a {
                        CallArg::Pos(e) => {
                            self.emit_expr(e)?;
                            specs.push((0u8, String::new()));
                        }
                        CallArg::Star(e) => {
                            self.emit_expr(e)?;
                            specs.push((1u8, String::new()));
                        }
                        CallArg::Kw(k, e) => {
                            self.emit_expr(e)?;
                            specs.push((2u8, k.clone()));
                        }
                        CallArg::KwStar(e) => {
                            self.emit_expr(e)?;
                            specs.push((3u8, String::new()));
                        }
                    }
                }
                self.code.push(Op::CallExt(kind, op_name, specs));
            }
        }
        Ok(())
    }
    pub(crate) fn emit_stmts(&mut self, ss: &[Stmt]) -> Result<(), String> {
        for s in ss {
            self.emit_stmt(s)?
        }
        Ok(())
    }
    fn emit_stmt(&mut self, s: &Stmt) -> Result<(), String> {
        match s {
            Stmt::Let(n, ty, e, c) => {
                self.emit_expr(e)?;
                self.emit_define(n, *c, ty.clone());
            }
            Stmt::Assign(n, e) => {
                self.emit_expr(e)?;
                self.emit_store(n);
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
                self.emit_define(name, false, None);
                let loop_start = self.code.len();
                self.emit_load(name);
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
                self.emit_load(name);
                self.code.push(Op::Const(Val::Num(1.0)));
                self.code.push(Op::Add);
                self.emit_store(name);
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
                self.emit_define(&iter_name, false, None);
                self.code.push(Op::Const(Val::Num(0.0)));
                self.emit_define(&idx_name, false, None);
                let loop_start = self.code.len();
                self.emit_load(&idx_name);
                self.emit_load(&iter_name);
                self.code.push(Op::Call("len".into(), 1));
                self.code.push(Op::Lt);
                let jf = self.code.len();
                self.code.push(Op::JumpIfFalse(0));
                self.emit_load(&iter_name);
                self.emit_load(&idx_name);
                self.code.push(Op::Index);
                self.emit_define(name, false, None);
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
                self.emit_load(&idx_name);
                self.code.push(Op::Const(Val::Num(1.0)));
                self.code.push(Op::Add);
                self.emit_store(&idx_name);
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
                self.emit_define(&subject_name, false, None);
                let mut end_jumps = vec![];
                let mut wildcard_seen = false;
                for (pattern, body) in cases {
                    if wildcard_seen {
                        return Err("match cases after wildcard '_' are unreachable".into());
                    }
                    if let Some(pattern) = pattern {
                        self.emit_load(&subject_name);
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
                let err_name = self.temp("with_err");
                self.emit_expr(resource)?;
                self.emit_define(&resource_name, false, None);
                self.emit_load(&resource_name);
                self.code.push(Op::CallMethod("enter".into(), 0));
                self.emit_define(name, false, None);
                let push = self.code.len();
                self.code.push(Op::PushHandler(0, err_name.clone()));
                self.emit_stmts(body)?;
                self.code.push(Op::PopHandler);
                self.emit_load(&resource_name);
                self.code.push(Op::CallMethod("exit".into(), 0));
                self.code.push(Op::Pop);
                let jump_end = self.code.len();
                self.code.push(Op::Jump(0));
                let handler_pc = self.code.len();
                self.code[push] = Op::PushHandler(handler_pc, err_name.clone());
                self.emit_load(&resource_name);
                self.code.push(Op::CallMethod("exit".into(), 0));
                self.code.push(Op::Pop);
                self.code.push(Op::Load(err_name));
                self.code.push(Op::Throw);
                let end_pc = self.code.len();
                self.code[jump_end] = Op::Jump(end_pc);
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
            Stmt::SetIndex(target, idx, value) => match target {
                Expr::Var(name) => {
                    self.emit_load(name);
                    self.emit_expr(idx)?;
                    self.emit_expr(value)?;
                    self.code.push(Op::SetIndex);
                    self.emit_store(name);
                }
                Expr::Member(obj, field) => {
                    self.emit_expr(obj)?;
                    self.emit_expr(target)?;
                    self.emit_expr(idx)?;
                    self.emit_expr(value)?;
                    self.code.push(Op::SetIndex);
                    self.code.push(Op::SetMember(field.clone()));
                }
                Expr::Index(outer_target, outer_idx) => {
                    if let Expr::Var(name) = outer_target.as_ref() {
                        self.emit_load(name);
                        self.emit_expr(outer_idx)?;
                        self.emit_expr(target)?;
                        self.emit_expr(idx)?;
                        self.emit_expr(value)?;
                        self.code.push(Op::SetIndex);
                        self.code.push(Op::SetIndex);
                        self.emit_store(name);
                    } else {
                        return Err("unsupported nested index assignment target".into());
                    }
                }
                _ => return Err("unsupported index assignment target".into()),
            },
            Stmt::Class(name, parent, body) => {
                let mut methods = vec![];
                let mut method_dbgs = vec![];
                let mut method_line = self.cur_line;
                for item in body {
                    match item {
                        Stmt::SourceLine(line) => {
                            method_line = *line;
                        }
                        Stmt::Fn(method, params, return_type, statements) => {
                            let (child_code, child_dbg) =
                                self.compile_child(params, statements, method_line)?;
                            method_dbgs.push((methods.len(), child_dbg));
                            methods.push((
                                method.clone(),
                                params.clone(),
                                return_type.clone(),
                                false,
                                child_code,
                            ));
                        }
                        Stmt::AsyncFn(method, params, return_type, statements) => {
                            let (child_code, child_dbg) =
                                self.compile_child(params, statements, method_line)?;
                            method_dbgs.push((methods.len(), child_dbg));
                            methods.push((
                                method.clone(),
                                params.clone(),
                                return_type.clone(),
                                true,
                                child_code,
                            ));
                        }
                        _ => return Err("class bodies currently contain methods only".into()),
                    }
                }
                let class_ip = self.code.len();
                for (sub_idx, dbg) in method_dbgs {
                    self.child_debugs.push((class_ip, sub_idx, dbg));
                }
                self.code
                    .push(Op::DefClass(name.clone(), parent.clone(), methods));
            }
            Stmt::Fn(name, params, return_type, body) => {
                let (child_code, child_dbg) = self.compile_child(params, body, self.cur_line)?;
                let def_ip = self.code.len();
                self.child_debugs.push((def_ip, 0, child_dbg));
                self.code.push(Op::Def(
                    name.clone(),
                    params.clone(),
                    return_type.clone(),
                    false,
                    child_code,
                ));
            }
            Stmt::DecoratedFn(decorator, name, params, return_type, body) => {
                let (child_code, child_dbg) = self.compile_child(params, body, self.cur_line)?;
                let def_ip = self.code.len();
                self.child_debugs.push((def_ip, 0, child_dbg));
                self.code.push(Op::Def(
                    name.clone(),
                    params.clone(),
                    return_type.clone(),
                    false,
                    child_code,
                ));
                self.code.push(Op::FunctionRef(name.clone()));
                self.code.push(Op::Call(decorator.clone(), 1));
                self.code
                    .push(Op::Define(name.clone(), false, Some("Function".into())));
            }
            Stmt::AsyncFn(name, params, return_type, body) => {
                let (child_code, child_dbg) = self.compile_child(params, body, self.cur_line)?;
                let def_ip = self.code.len();
                self.child_debugs.push((def_ip, 0, child_dbg));
                self.code.push(Op::Def(
                    name.clone(),
                    params.clone(),
                    return_type.clone(),
                    true,
                    child_code,
                ));
            }
            Stmt::SourceLine(line) => {
                self.cur_line = *line;
                self.push_line_mark(*line);
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
    pub(crate) fn finish(mut self) -> Vec<Op> {
        self.code.push(Op::Halt);
        self.code
    }
    pub(crate) fn finish_with_debug(mut self) -> (Vec<Op>, CompiledDebug) {
        self.code.push(Op::Halt);
        let dbg = CompiledDebug {
            lines: self.line_marks,
            children: self.child_debugs,
            local_names: self.slot_names,
        };
        (self.code, dbg)
    }
}
