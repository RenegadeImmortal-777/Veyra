// Bytecode compiler: AST to `Op` stream.
//
// Extracted from `main.rs` without behavior change; see docs/ENGINE-SPLIT.md.

use crate::ast::{Expr, Stmt};
use crate::bytecode::Op;
use crate::value::Val;

struct LoopCtx {
    breaks: Vec<usize>,
    continues: Vec<usize>,
}

pub(crate) struct Compiler {
    code: Vec<Op>,
    next_temp: usize,
    loops: Vec<LoopCtx>,
}
impl Compiler {
    pub(crate) fn new() -> Self {
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
            Expr::Call(n, args) => {
                for a in args {
                    self.emit_expr(a)?
                }
                self.code.push(Op::Call(n.clone(), args.len()));
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
                let err_name = self.temp("with_err");
                self.emit_expr(resource)?;
                self.code
                    .push(Op::Define(resource_name.clone(), false, None));
                self.code.push(Op::Load(resource_name.clone()));
                self.code.push(Op::CallMethod("enter".into(), 0));
                self.code.push(Op::Define(name.clone(), false, None));
                let push = self.code.len();
                self.code.push(Op::PushHandler(0, err_name.clone()));
                self.emit_stmts(body)?;
                self.code.push(Op::PopHandler);
                self.code.push(Op::Load(resource_name.clone()));
                self.code.push(Op::CallMethod("exit".into(), 0));
                self.code.push(Op::Pop);
                let jump_end = self.code.len();
                self.code.push(Op::Jump(0));
                let handler_pc = self.code.len();
                self.code[push] = Op::PushHandler(handler_pc, err_name.clone());
                self.code.push(Op::Load(resource_name));
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
                    self.code.push(Op::Load(name.clone()));
                    self.emit_expr(idx)?;
                    self.emit_expr(value)?;
                    self.code.push(Op::SetIndex);
                    self.code.push(Op::Store(name.clone()));
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
                        self.code.push(Op::Load(name.clone()));
                        self.emit_expr(outer_idx)?;
                        self.emit_expr(target)?;
                        self.emit_expr(idx)?;
                        self.emit_expr(value)?;
                        self.code.push(Op::SetIndex);
                        self.code.push(Op::SetIndex);
                        self.code.push(Op::Store(name.clone()));
                    } else {
                        return Err("unsupported nested index assignment target".into());
                    }
                }
                _ => return Err("unsupported index assignment target".into()),
            },
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
    pub(crate) fn finish(mut self) -> Vec<Op> {
        self.code.push(Op::Halt);
        self.code
    }
}
