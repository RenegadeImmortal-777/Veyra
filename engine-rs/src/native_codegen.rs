//! Standalone binary code generators and zero-dependency execution engines:
//! 1. Pure-Rust WebAssembly 1.0 (`.wasm`) compiler, disassembler, and stack VM runner.
//! 2. Pure-Rust direct `x86_64` PE32+ (Windows) and ELF64 (Linux) executable emitter.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use crate::ast::{CallArg, Expr, Stmt};
use crate::lexer::lex_with_lines;
use crate::parser::Parser;
use crate::pipeline::{load_source, semantic_check_ast};

// ============================================================================
// LEB128 & Binary Cursor Helpers
// ============================================================================

fn push_uleb128(out: &mut Vec<u8>, mut v: u32) {
    loop {
        let mut byte = (v & 0x7F) as u8;
        v >>= 7;
        if v != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if v == 0 {
            break;
        }
    }
}

fn push_wasm_name(out: &mut Vec<u8>, s: &str) {
    push_uleb128(out, s.len() as u32);
    out.extend_from_slice(s.as_bytes());
}

fn push_wasm_section(out: &mut Vec<u8>, id: u8, payload: &[u8]) {
    if payload.is_empty() {
        return;
    }
    out.push(id);
    push_uleb128(out, payload.len() as u32);
    out.extend_from_slice(payload);
}

fn read_uleb128(bytes: &[u8], pos: &mut usize) -> Result<u32, String> {
    let mut res = 0u32;
    let mut shift = 0u32;
    for _ in 0..5 {
        let b = *bytes.get(*pos).ok_or("wasm: unexpected EOF in uleb128")?;
        *pos += 1;
        res |= u32::from(b & 0x7F) << shift;
        if (b & 0x80) == 0 {
            return Ok(res);
        }
        shift += 7;
    }
    Err("wasm: uleb128 overflow".into())
}

fn read_sleb128(bytes: &[u8], pos: &mut usize) -> Result<i64, String> {
    let mut res = 0i64;
    let mut shift = 0u32;
    let mut byte;
    loop {
        byte = *bytes.get(*pos).ok_or("wasm: unexpected EOF in sleb128")?;
        *pos += 1;
        res |= i64::from(byte & 0x7F) << shift;
        shift += 7;
        if (byte & 0x80) == 0 || shift >= 64 {
            break;
        }
    }
    if shift < 64 && (byte & 0x40) != 0 {
        res |= !0i64 << shift;
    }
    Ok(res)
}

fn read_wasm_name(bytes: &[u8], pos: &mut usize) -> Result<String, String> {
    let len = read_uleb128(bytes, pos)? as usize;
    let end = pos.saturating_add(len);
    let slice = bytes
        .get(*pos..end)
        .ok_or("wasm: unexpected EOF reading name")?;
    *pos = end;
    String::from_utf8(slice.to_vec()).map_err(|_| "wasm: invalid UTF-8 name".into())
}

// ============================================================================
// Part A1: Pure-Rust AST -> WebAssembly 1.0 Binary Compiler
// ============================================================================

const WASM_VAL_I32: u8 = 0x7F;
const WASM_VAL_F64: u8 = 0x7C;
const WASM_VOID: u8 = 0x40;

#[derive(Clone)]
struct WasmFnInfo {
    name: String,
    params: Vec<String>,
    body: Vec<Stmt>,
    func_index: u32,
    type_index: u32,
}

#[derive(Clone, Copy)]
struct WasmLoopLabels {
    break_depth: u32,
    continue_depth: u32,
}

struct WasmCodeEmitter<'a> {
    code: Vec<u8>,
    locals: HashMap<String, u32>,
    next_local: u32,
    globals: &'a HashMap<String, u32>,
    funcs: &'a HashMap<String, WasmFnInfo>,
    print_fn_idx: Option<u32>,
    pow_fn_idx: Option<u32>,
    scope_depth: u32,
    loops: Vec<WasmLoopLabels>,
    is_top_level: bool,
}

impl<'a> WasmCodeEmitter<'a> {
    fn new(
        params: &[String],
        globals: &'a HashMap<String, u32>,
        funcs: &'a HashMap<String, WasmFnInfo>,
        print_fn_idx: Option<u32>,
        pow_fn_idx: Option<u32>,
        is_top_level: bool,
    ) -> Self {
        let mut locals = HashMap::new();
        for (i, p) in params.iter().enumerate() {
            locals.insert(p.clone(), i as u32);
        }
        Self {
            code: Vec::new(),
            locals,
            next_local: params.len() as u32,
            globals,
            funcs,
            print_fn_idx,
            pow_fn_idx,
            scope_depth: 0,
            loops: Vec::new(),
            is_top_level,
        }
    }

    fn alloc_temp(&mut self) -> u32 {
        let idx = self.next_local;
        self.next_local += 1;
        idx
    }

    fn get_or_define_local(&mut self, name: &str) -> u32 {
        if let Some(&idx) = self.locals.get(name) {
            idx
        } else {
            let idx = self.alloc_temp();
            self.locals.insert(name.to_string(), idx);
            idx
        }
    }

    fn emit_f64_const(&mut self, v: f64) {
        self.code.push(0x44);
        self.code.extend_from_slice(&v.to_le_bytes());
    }

    fn emit_local_get(&mut self, idx: u32) {
        self.code.push(0x20);
        push_uleb128(&mut self.code, idx);
    }

    fn emit_local_set(&mut self, idx: u32) {
        self.code.push(0x21);
        push_uleb128(&mut self.code, idx);
    }

    fn emit_local_tee(&mut self, idx: u32) {
        self.code.push(0x22);
        push_uleb128(&mut self.code, idx);
    }

    fn emit_global_get(&mut self, idx: u32) {
        self.code.push(0x23);
        push_uleb128(&mut self.code, idx);
    }

    fn emit_global_set(&mut self, idx: u32) {
        self.code.push(0x24);
        push_uleb128(&mut self.code, idx);
    }

    fn emit_call(&mut self, idx: u32) {
        self.code.push(0x10);
        push_uleb128(&mut self.code, idx);
    }

    fn emit_br(&mut self, depth: u32) {
        self.code.push(0x0C);
        push_uleb128(&mut self.code, depth);
    }

    fn emit_br_if(&mut self, depth: u32) {
        self.code.push(0x0D);
        push_uleb128(&mut self.code, depth);
    }

    fn emit_truthy_i32(&mut self, expr: &Expr) -> Result<(), String> {
        self.compile_expr(expr)?;
        self.emit_f64_const(0.0);
        self.code.push(0x62); // f64.ne -> i32
        Ok(())
    }

    fn compile_expr(&mut self, expr: &Expr) -> Result<(), String> {
        match expr {
            Expr::Num(n) => self.emit_f64_const(*n),
            Expr::Bool(b) => self.emit_f64_const(if *b { 1.0 } else { 0.0 }),
            Expr::Null => self.emit_f64_const(0.0),
            Expr::Var(name) => {
                if let Some(&idx) = self.locals.get(name) {
                    self.emit_local_get(idx);
                } else if let Some(&gidx) = self.globals.get(name) {
                    self.emit_global_get(gidx);
                } else {
                    return Err(format!("wasm: undefined variable '{name}'"));
                }
            }
            Expr::Unary(op, inner) => {
                self.compile_expr(inner)?;
                match op.as_str() {
                    "-" => self.code.push(0x9A), // f64.neg
                    "+" => {}
                    "not" | "!" => {
                        self.emit_f64_const(0.0);
                        self.code.push(0x61); // f64.eq
                        self.code.push(0xB7); // f64.convert_i32_s
                    }
                    _ => return Err(format!("wasm: unsupported unary op '{op}'")),
                }
            }
            Expr::Bin(lhs, op, rhs) => match op.as_str() {
                "and" => {
                    let tmp = self.alloc_temp();
                    self.compile_expr(lhs)?;
                    self.emit_local_tee(tmp);
                    self.emit_f64_const(0.0);
                    self.code.push(0x62); // f64.ne
                    self.code.extend_from_slice(&[0x04, WASM_VAL_F64]); // if (result f64)
                    self.scope_depth += 1;
                    self.compile_expr(rhs)?;
                    self.code.push(0x05); // else
                    self.emit_local_get(tmp);
                    self.code.push(0x0B); // end
                    self.scope_depth -= 1;
                }
                "or" => {
                    let tmp = self.alloc_temp();
                    self.compile_expr(lhs)?;
                    self.emit_local_tee(tmp);
                    self.emit_f64_const(0.0);
                    self.code.push(0x62); // f64.ne
                    self.code.extend_from_slice(&[0x04, WASM_VAL_F64]);
                    self.scope_depth += 1;
                    self.emit_local_get(tmp);
                    self.code.push(0x05);
                    self.compile_expr(rhs)?;
                    self.code.push(0x0B);
                    self.scope_depth -= 1;
                }
                "%" => {
                    let ta = self.alloc_temp();
                    let tb = self.alloc_temp();
                    self.compile_expr(lhs)?;
                    self.emit_local_set(ta);
                    self.compile_expr(rhs)?;
                    self.emit_local_set(tb);
                    self.emit_local_get(ta);
                    self.emit_local_get(ta);
                    self.emit_local_get(tb);
                    self.code.push(0xA3); // f64.div
                    self.code.push(0x9D); // f64.trunc
                    self.emit_local_get(tb);
                    self.code.push(0xA2); // f64.mul
                    self.code.push(0xA1); // f64.sub
                }
                "**" => {
                    let pow_idx = self.pow_fn_idx.ok_or("wasm: internal pow helper missing")?;
                    self.compile_expr(lhs)?;
                    self.compile_expr(rhs)?;
                    self.emit_call(pow_idx);
                }
                "+" | "-" | "*" | "/" | "==" | "!=" | "<" | "<=" | ">" | ">=" => {
                    self.compile_expr(lhs)?;
                    self.compile_expr(rhs)?;
                    match op.as_str() {
                        "+" => self.code.push(0xA0),
                        "-" => self.code.push(0xA1),
                        "*" => self.code.push(0xA2),
                        "/" => self.code.push(0xA3),
                        "==" => {
                            self.code.push(0x61);
                            self.code.push(0xB7);
                        }
                        "!=" => {
                            self.code.push(0x62);
                            self.code.push(0xB7);
                        }
                        "<" => {
                            self.code.push(0x63);
                            self.code.push(0xB7);
                        }
                        ">" => {
                            self.code.push(0x64);
                            self.code.push(0xB7);
                        }
                        "<=" => {
                            self.code.push(0x65);
                            self.code.push(0xB7);
                        }
                        ">=" => {
                            self.code.push(0x66);
                            self.code.push(0xB7);
                        }
                        _ => unreachable!(),
                    }
                }
                _ => return Err(format!("wasm: unsupported binary op '{op}'")),
            },
            Expr::Ternary(cond, t, e) => {
                self.emit_truthy_i32(cond)?;
                self.code.extend_from_slice(&[0x04, WASM_VAL_F64]);
                self.scope_depth += 1;
                self.compile_expr(t)?;
                self.code.push(0x05);
                self.compile_expr(e)?;
                self.code.push(0x0B);
                self.scope_depth -= 1;
            }
            Expr::ChainedCmp(first, rest) => {
                if rest.is_empty() {
                    self.compile_expr(first)?;
                    return Ok(());
                }
                let mut lhs_tmp = self.alloc_temp();
                self.compile_expr(first)?;
                self.emit_local_set(lhs_tmp);
                let acc_tmp = self.alloc_temp();
                self.emit_f64_const(1.0);
                self.emit_local_set(acc_tmp);
                for (cmp_op, rhs) in rest {
                    let rhs_tmp = self.alloc_temp();
                    self.compile_expr(rhs)?;
                    self.emit_local_set(rhs_tmp);
                    self.emit_local_get(lhs_tmp);
                    self.emit_local_get(rhs_tmp);
                    match cmp_op.as_str() {
                        "==" => self.code.push(0x61),
                        "!=" => self.code.push(0x62),
                        "<" => self.code.push(0x63),
                        ">" => self.code.push(0x64),
                        "<=" => self.code.push(0x65),
                        ">=" => self.code.push(0x66),
                        _ => return Err(format!("wasm: unsupported chained cmp '{cmp_op}'")),
                    }
                    self.code.push(0xB7); // f64.convert_i32_s
                    self.emit_local_get(acc_tmp);
                    self.code.push(0xA2); // f64.mul
                    self.emit_local_set(acc_tmp);
                    lhs_tmp = rhs_tmp;
                }
                self.emit_local_get(acc_tmp);
            }
            Expr::Call(name, args) => self.compile_call_expr(name, args)?,
            Expr::CallExt {
                callee,
                method: None,
                args,
            } => {
                let Expr::Var(name) = callee.as_ref() else {
                    return Err("wasm: dynamic callee calls not supported".into());
                };
                let mut pos_args = Vec::with_capacity(args.len());
                for a in args {
                    match a {
                        CallArg::Pos(e) => pos_args.push(e.clone()),
                        _ => return Err("wasm: only positional call args supported".into()),
                    }
                }
                self.compile_call_expr(name, &pos_args)?;
            }
            _ => {
                return Err(format!(
                    "wasm: unsupported expression '{expr:?}' (numeric scalar subset supported)"
                ));
            }
        }
        Ok(())
    }

    fn compile_call_expr(&mut self, name: &str, args: &[Expr]) -> Result<(), String> {
        match name {
            "abs" if args.len() == 1 => {
                self.compile_expr(&args[0])?;
                self.code.push(0x99);
            }
            "ceil" if args.len() == 1 => {
                self.compile_expr(&args[0])?;
                self.code.push(0x9B);
            }
            "floor" if args.len() == 1 => {
                self.compile_expr(&args[0])?;
                self.code.push(0x9C);
            }
            "round" if args.len() == 1 => {
                self.compile_expr(&args[0])?;
                self.code.push(0x9E);
            }
            "sqrt" if args.len() == 1 => {
                self.compile_expr(&args[0])?;
                self.code.push(0x9F);
            }
            "min" if args.len() == 2 => {
                self.compile_expr(&args[0])?;
                self.compile_expr(&args[1])?;
                self.code.push(0xA4);
            }
            "max" if args.len() == 2 => {
                self.compile_expr(&args[0])?;
                self.compile_expr(&args[1])?;
                self.code.push(0xA5);
            }
            "pow" if args.len() == 2 => {
                let pow_idx = self.pow_fn_idx.ok_or("wasm: internal pow helper missing")?;
                self.compile_expr(&args[0])?;
                self.compile_expr(&args[1])?;
                self.emit_call(pow_idx);
            }
            "print" | "println" => {
                let p_idx = self
                    .print_fn_idx
                    .ok_or("wasm: print function import not available")?;
                for a in args {
                    self.compile_expr(a)?;
                    self.emit_call(p_idx);
                }
                self.emit_f64_const(0.0);
            }
            "requires" | "ensures" => {
                if let Some(cond) = args.first() {
                    self.emit_truthy_i32(cond)?;
                    self.code.push(0x45); // i32.eqz
                    self.code.extend_from_slice(&[0x04, WASM_VOID]);
                    self.scope_depth += 1;
                    self.code.push(0x00); // unreachable (trap)
                    self.code.push(0x0B);
                    self.scope_depth -= 1;
                }
                self.emit_f64_const(0.0);
            }
            "assert" => {
                if let Some(cond) = args.first() {
                    self.emit_truthy_i32(cond)?;
                    self.code.push(0x45);
                    self.code.extend_from_slice(&[0x04, WASM_VOID]);
                    self.scope_depth += 1;
                    self.code.push(0x00);
                    self.code.push(0x0B);
                    self.scope_depth -= 1;
                }
                self.emit_f64_const(0.0);
            }
            "int" | "float" | "num" if args.len() == 1 => {
                self.compile_expr(&args[0])?;
                if name == "int" {
                    self.code.push(0x9D); // f64.trunc
                }
            }
            _ => {
                let info = self
                    .funcs
                    .get(name)
                    .ok_or_else(|| format!("wasm: unsupported call '{name}'"))?;
                if info.params.len() != args.len() {
                    return Err(format!(
                        "wasm: function '{name}' expects {} args, got {}",
                        info.params.len(),
                        args.len()
                    ));
                }
                for a in args {
                    self.compile_expr(a)?;
                }
                self.emit_call(info.func_index);
            }
        }
        Ok(())
    }

    fn compile_stmt(&mut self, stmt: &Stmt) -> Result<(), String> {
        match stmt {
            Stmt::SourceLine(_) | Stmt::Fn(..) | Stmt::AsyncFn(..) | Stmt::DecoratedFn(..) => {}
            Stmt::Let(name, _, val, _) => {
                self.compile_expr(val)?;
                if self.is_top_level
                    && let Some(&gidx) = self.globals.get(name)
                {
                    self.emit_global_set(gidx);
                } else {
                    let lidx = self.get_or_define_local(name);
                    self.emit_local_set(lidx);
                }
            }
            Stmt::Assign(name, val) => {
                self.compile_expr(val)?;
                if let Some(&lidx) = self.locals.get(name) {
                    self.emit_local_set(lidx);
                } else if let Some(&gidx) = self.globals.get(name) {
                    self.emit_global_set(gidx);
                } else {
                    return Err(format!("wasm: assignment to undefined '{name}'"));
                }
            }
            Stmt::Expr(e) => {
                self.compile_expr(e)?;
                self.code.push(0x1A); // drop
            }
            Stmt::If(cond, then_b, else_b) => {
                self.emit_truthy_i32(cond)?;
                self.code.extend_from_slice(&[0x04, WASM_VOID]);
                self.scope_depth += 1;
                for s in then_b {
                    self.compile_stmt(s)?;
                }
                if !else_b.is_empty() {
                    self.code.push(0x05);
                    for s in else_b {
                        self.compile_stmt(s)?;
                    }
                }
                self.code.push(0x0B);
                self.scope_depth -= 1;
            }
            Stmt::While(cond, body) => {
                let break_depth = self.scope_depth;
                self.code.extend_from_slice(&[0x02, WASM_VOID]); // block $break
                self.scope_depth += 1;
                let cont_depth = self.scope_depth;
                self.code.extend_from_slice(&[0x03, WASM_VOID]); // loop $cont
                self.scope_depth += 1;
                self.emit_truthy_i32(cond)?;
                self.code.push(0x45); // i32.eqz
                self.emit_br_if(1); // break outer block
                self.loops.push(WasmLoopLabels {
                    break_depth,
                    continue_depth: cont_depth,
                });
                for s in body {
                    self.compile_stmt(s)?;
                }
                self.loops.pop();
                self.emit_br(0); // continue loop
                self.code.push(0x0B); // end loop
                self.scope_depth -= 1;
                self.code.push(0x0B); // end block
                self.scope_depth -= 1;
            }
            Stmt::ForRange(var, start, end_expr, body) => {
                let var_slot = self.get_or_define_local(var);
                let end_slot = self.alloc_temp();
                self.compile_expr(start)?;
                self.emit_local_set(var_slot);
                self.compile_expr(end_expr)?;
                self.emit_local_set(end_slot);

                let break_depth = self.scope_depth;
                self.code.extend_from_slice(&[0x02, WASM_VOID]); // block $break
                self.scope_depth += 1;
                let loop_depth = self.scope_depth;
                self.code.extend_from_slice(&[0x03, WASM_VOID]); // loop $top
                self.scope_depth += 1;
                self.emit_local_get(var_slot);
                self.emit_local_get(end_slot);
                self.code.push(0x63); // f64.lt
                self.code.push(0x45); // i32.eqz
                self.emit_br_if(1);

                // Inner block lets `continue` skip remaining body statements while STILL executing step += 1.0!
                let cont_depth = self.scope_depth;
                self.code.extend_from_slice(&[0x02, WASM_VOID]); // block $cont
                self.scope_depth += 1;
                self.loops.push(WasmLoopLabels {
                    break_depth,
                    continue_depth: cont_depth,
                });
                for s in body {
                    self.compile_stmt(s)?;
                }
                self.loops.pop();
                self.code.push(0x0B); // end $cont
                self.scope_depth -= 1;

                // Increment Induction Var
                self.emit_local_get(var_slot);
                self.emit_f64_const(1.0);
                self.code.push(0xA0); // f64.add
                self.emit_local_set(var_slot);
                self.emit_br(loop_depth - break_depth - 1); // br 0 -> loop $top
                self.code.push(0x0B); // end $top
                self.scope_depth -= 1;
                self.code.push(0x0B); // end $break
                self.scope_depth -= 1;
            }
            Stmt::Break => {
                let top = self.loops.last().ok_or("wasm: break outside loop")?;
                let rel = self.scope_depth - top.break_depth - 1;
                self.emit_br(rel);
            }
            Stmt::Continue => {
                let top = self.loops.last().ok_or("wasm: continue outside loop")?;
                let rel = self.scope_depth - top.continue_depth - 1;
                self.emit_br(rel);
            }
            Stmt::Return(Some(e)) => {
                self.compile_expr(e)?;
                self.code.push(0x0F);
            }
            Stmt::Return(None) => {
                self.emit_f64_const(0.0);
                self.code.push(0x0F);
            }
            _ => {
                return Err(format!(
                    "wasm: unsupported statement '{stmt:?}' (numeric subset supported)"
                ));
            }
        }
        Ok(())
    }

    fn finish_body(mut self, stmts: &[Stmt], param_count: u32) -> Result<Vec<u8>, String> {
        for s in stmts {
            self.compile_stmt(s)?;
        }
        self.emit_f64_const(0.0);
        self.code.push(0x0B); // end function

        let extra_locals = self.next_local.saturating_sub(param_count);
        let mut body = Vec::new();
        if extra_locals > 0 {
            push_uleb128(&mut body, 1);
            push_uleb128(&mut body, extra_locals);
            body.push(WASM_VAL_F64);
        } else {
            push_uleb128(&mut body, 0);
        }
        body.extend_from_slice(&self.code);
        Ok(body)
    }
}

fn stmt_list_uses_op(stmts: &[Stmt], need_print: &mut bool, need_pow: &mut bool) {
    fn walk_expr(e: &Expr, need_print: &mut bool, need_pow: &mut bool) {
        match e {
            Expr::Call(n, args) => {
                if n == "print" || n == "println" {
                    *need_print = true;
                } else if n == "pow" {
                    *need_pow = true;
                }
                for a in args {
                    walk_expr(a, need_print, need_pow);
                }
            }
            Expr::Bin(l, op, r) => {
                if op == "**" {
                    *need_pow = true;
                }
                walk_expr(l, need_print, need_pow);
                walk_expr(r, need_print, need_pow);
            }
            Expr::Unary(_, x) => walk_expr(x, need_print, need_pow),
            Expr::Ternary(a, b, c) => {
                walk_expr(a, need_print, need_pow);
                walk_expr(b, need_print, need_pow);
                walk_expr(c, need_print, need_pow);
            }
            Expr::ChainedCmp(a, rest) => {
                walk_expr(a, need_print, need_pow);
                for (_, r) in rest {
                    walk_expr(r, need_print, need_pow);
                }
            }
            Expr::CallExt { callee, args, .. } => {
                if let Expr::Var(n) = callee.as_ref() {
                    if n == "print" || n == "println" {
                        *need_print = true;
                    } else if n == "pow" {
                        *need_pow = true;
                    }
                }
                for a in args {
                    if let CallArg::Pos(e) = a {
                        walk_expr(e, need_print, need_pow);
                    }
                }
            }
            _ => {}
        }
    }
    for s in stmts {
        match s {
            Stmt::SourceLine(_) | Stmt::Break | Stmt::Continue => {}
            Stmt::Let(_, _, e, _) | Stmt::Assign(_, e) | Stmt::Expr(e) => {
                walk_expr(e, need_print, need_pow)
            }
            Stmt::Return(Some(e)) | Stmt::Yield(e) | Stmt::Throw(e) => {
                walk_expr(e, need_print, need_pow)
            }
            Stmt::If(c, t, f) => {
                walk_expr(c, need_print, need_pow);
                stmt_list_uses_op(t, need_print, need_pow);
                stmt_list_uses_op(f, need_print, need_pow);
            }
            Stmt::While(c, b) => {
                walk_expr(c, need_print, need_pow);
                stmt_list_uses_op(b, need_print, need_pow);
            }
            Stmt::ForRange(_, s, e, b) => {
                walk_expr(s, need_print, need_pow);
                walk_expr(e, need_print, need_pow);
                stmt_list_uses_op(b, need_print, need_pow);
            }
            Stmt::Fn(_, _, _, b) | Stmt::AsyncFn(_, _, _, b) | Stmt::DecoratedFn(_, _, _, _, b) => {
                stmt_list_uses_op(b, need_print, need_pow)
            }
            _ => {}
        }
    }
}

fn emit_wasm_pow_helper_body() -> Vec<u8> {
    // fn __vy_pow(base: f64 [0], exp: f64 [1]) -> f64; local[2] = acc, local[3] = i
    // Computes integer loop exp = trunc(exp); acc = 1.0; handles negative exp.
    let mut b = Vec::new();
    push_uleb128(&mut b, 1);
    push_uleb128(&mut b, 3);
    b.push(WASM_VAL_F64); // locals 2(acc), 3(n), 4(neg)

    // neg = exp < 0.0
    b.extend_from_slice(&[0x20, 0x01, 0x44]);
    b.extend_from_slice(&0.0f64.to_le_bytes());
    b.extend_from_slice(&[0x63, 0xB7, 0x21, 0x04]); // local.set 4

    // n = trunc(abs(exp))
    b.extend_from_slice(&[0x20, 0x01, 0x99, 0x9D, 0x21, 0x03]);

    // acc = 1.0
    b.push(0x44);
    b.extend_from_slice(&1.0f64.to_le_bytes());
    b.extend_from_slice(&[0x21, 0x02]);

    // loop while n > 0.0
    b.extend_from_slice(&[0x02, WASM_VOID, 0x03, WASM_VOID]);
    b.extend_from_slice(&[0x20, 0x03, 0x44]);
    b.extend_from_slice(&0.0f64.to_le_bytes());
    b.extend_from_slice(&[0x66, 0x0D, 0x01]); // n <= 0.0 => br_if 1
    b.extend_from_slice(&[0x20, 0x02, 0x20, 0x00, 0xA2, 0x21, 0x02]); // acc *= base
    b.extend_from_slice(&[0x20, 0x03, 0x44]);
    b.extend_from_slice(&1.0f64.to_le_bytes());
    b.extend_from_slice(&[0xA1, 0x21, 0x03]); // n -= 1
    b.extend_from_slice(&[0x0C, 0x00, 0x0B, 0x0B]);

    // if neg != 0: acc = 1.0 / acc
    b.extend_from_slice(&[0x20, 0x04, 0x44]);
    b.extend_from_slice(&0.0f64.to_le_bytes());
    b.extend_from_slice(&[0x62, 0x04, WASM_VOID]);
    b.push(0x44);
    b.extend_from_slice(&1.0f64.to_le_bytes());
    b.extend_from_slice(&[0x20, 0x02, 0xA3, 0x21, 0x02, 0x0B]);

    b.extend_from_slice(&[0x20, 0x02, 0x0B]); // return acc
    b
}

pub(crate) fn compile_source_to_wasm(src: &str) -> Result<Vec<u8>, String> {
    let tokens = lex_with_lines(src)?;
    let ast = Parser::new_with_lines(tokens).program()?;
    semantic_check_ast(&ast)?;

    let mut need_print = false;
    let mut need_pow = false;
    stmt_list_uses_op(&ast, &mut need_print, &mut need_pow);

    let mut globals_order: Vec<String> = Vec::new();
    let mut globals_map: HashMap<String, u32> = HashMap::new();
    let mut user_defs: Vec<(String, Vec<String>, Vec<Stmt>)> = Vec::new();
    let mut top_stmts: Vec<Stmt> = Vec::new();

    for s in ast {
        match s {
            Stmt::Fn(name, params, _, body)
            | Stmt::AsyncFn(name, params, _, body)
            | Stmt::DecoratedFn(_, name, params, _, body) => {
                user_defs.push((name, params.into_iter().map(|(p, _)| p).collect(), body));
            }
            Stmt::Let(ref name, _, _, _) => {
                if !globals_map.contains_key(name) {
                    let idx = globals_order.len() as u32;
                    globals_order.push(name.clone());
                    globals_map.insert(name.clone(), idx);
                }
                top_stmts.push(s);
            }
            other => top_stmts.push(other),
        }
    }

    // Type table deduplication: each signature (param_count, has_result)
    let mut type_entries: Vec<(usize, bool)> = Vec::new();
    let mut ensure_type = |arity: usize, has_ret: bool| -> u32 {
        if let Some(pos) = type_entries
            .iter()
            .position(|&(a, r)| a == arity && r == has_ret)
        {
            pos as u32
        } else {
            let idx = type_entries.len() as u32;
            type_entries.push((arity, has_ret));
            idx
        }
    };

    let print_type_idx = if need_print {
        Some(ensure_type(1, false))
    } else {
        None
    };
    let pow_type_idx = if need_pow {
        Some(ensure_type(2, true))
    } else {
        None
    };
    let main_type_idx = ensure_type(0, true);

    let mut next_func_idx = 0u32;
    let print_fn_idx = if need_print {
        let idx = next_func_idx;
        next_func_idx += 1;
        Some(idx)
    } else {
        None
    };
    let pow_fn_idx = if need_pow {
        let idx = next_func_idx;
        next_func_idx += 1;
        Some(idx)
    } else {
        None
    };

    let mut func_map = HashMap::new();
    let mut user_infos = Vec::new();
    for (name, params, body) in user_defs {
        let tid = ensure_type(params.len(), true);
        let fid = next_func_idx;
        next_func_idx += 1;
        let info = WasmFnInfo {
            name: name.clone(),
            params,
            body,
            func_index: fid,
            type_index: tid,
        };
        func_map.insert(name, info.clone());
        user_infos.push(info);
    }
    let main_fn_idx = next_func_idx;

    // 1. Type section
    let mut type_sec = Vec::new();
    push_uleb128(&mut type_sec, type_entries.len() as u32);
    for &(arity, has_ret) in &type_entries {
        type_sec.push(0x60);
        push_uleb128(&mut type_sec, arity as u32);
        type_sec.extend(std::iter::repeat_n(WASM_VAL_F64, arity));
        if has_ret {
            push_uleb128(&mut type_sec, 1);
            type_sec.push(WASM_VAL_F64);
        } else {
            push_uleb128(&mut type_sec, 0);
        }
    }

    // 2. Import section
    let mut import_sec = Vec::new();
    if let Some(tid) = print_type_idx {
        push_uleb128(&mut import_sec, 1);
        push_wasm_name(&mut import_sec, "env");
        push_wasm_name(&mut import_sec, "print_num");
        import_sec.push(0x00); // func import
        push_uleb128(&mut import_sec, tid);
    }

    // 3. Function section (internal functions: pow? + user + main)
    let mut func_sec = Vec::new();
    let internal_count = (if pow_type_idx.is_some() { 1 } else { 0 }) + user_infos.len() + 1;
    push_uleb128(&mut func_sec, internal_count as u32);
    if let Some(tid) = pow_type_idx {
        push_uleb128(&mut func_sec, tid);
    }
    for info in &user_infos {
        push_uleb128(&mut func_sec, info.type_index);
    }
    push_uleb128(&mut func_sec, main_type_idx);

    // 6. Global section
    let mut global_sec = Vec::new();
    if !globals_order.is_empty() {
        push_uleb128(&mut global_sec, globals_order.len() as u32);
        for _ in &globals_order {
            global_sec.push(WASM_VAL_F64);
            global_sec.push(0x01); // mut
            global_sec.push(0x44); // f64.const
            global_sec.extend_from_slice(&0.0f64.to_le_bytes());
            global_sec.push(0x0B); // end
        }
    }

    // 7. Export section (all user functions + main)
    let mut export_sec = Vec::new();
    push_uleb128(&mut export_sec, (user_infos.len() + 1) as u32);
    for info in &user_infos {
        push_wasm_name(&mut export_sec, &info.name);
        export_sec.push(0x00);
        push_uleb128(&mut export_sec, info.func_index);
    }
    push_wasm_name(&mut export_sec, "main");
    export_sec.push(0x00);
    push_uleb128(&mut export_sec, main_fn_idx);

    // 10. Code section
    let mut bodies: Vec<Vec<u8>> = Vec::with_capacity(internal_count);
    if need_pow {
        bodies.push(emit_wasm_pow_helper_body());
    }
    for info in &user_infos {
        let emitter = WasmCodeEmitter::new(
            &info.params,
            &globals_map,
            &func_map,
            print_fn_idx,
            pow_fn_idx,
            false,
        );
        bodies.push(emitter.finish_body(&info.body, info.params.len() as u32)?);
    }
    let main_emitter =
        WasmCodeEmitter::new(&[], &globals_map, &func_map, print_fn_idx, pow_fn_idx, true);
    bodies.push(main_emitter.finish_body(&top_stmts, 0)?);

    let mut code_sec = Vec::new();
    push_uleb128(&mut code_sec, bodies.len() as u32);
    for b in &bodies {
        push_uleb128(&mut code_sec, b.len() as u32);
        code_sec.extend_from_slice(b);
    }

    let mut wasm = Vec::from(&b"\0asm\x01\0\0\0"[..]);
    push_wasm_section(&mut wasm, 1, &type_sec);
    push_wasm_section(&mut wasm, 2, &import_sec);
    push_wasm_section(&mut wasm, 3, &func_sec);
    push_wasm_section(&mut wasm, 6, &global_sec);
    push_wasm_section(&mut wasm, 7, &export_sec);
    push_wasm_section(&mut wasm, 10, &code_sec);
    Ok(wasm)
}

pub(crate) fn compile_file_to_wasm(input: &Path, output: &Path) -> Result<(), String> {
    let src = load_source(input.to_string_lossy().as_ref())?;
    let bytes = compile_source_to_wasm(&src)?;
    if let Some(parent) = output.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::write(output, &bytes).map_err(|e| e.to_string())
}

// ============================================================================
// Part A2: Pure-Rust WebAssembly Binary Parser, Disassembler & Stack VM
// ============================================================================

#[derive(Clone, Debug)]
struct WasmTypeSig {
    params: Vec<u8>,
    results: Vec<u8>,
}

#[derive(Clone, Debug)]
enum WasmInsn {
    Unreachable,
    Nop,
    Block {
        block_type: u8,
        end_idx: usize,
    },
    Loop {
        block_type: u8,
        end_idx: usize,
    },
    If {
        block_type: u8,
        else_idx: Option<usize>,
        end_idx: usize,
    },
    Else {
        end_idx: usize,
    },
    End,
    Br(u32),
    BrIf(u32),
    Return,
    Call(u32),
    Drop,
    LocalGet(u32),
    LocalSet(u32),
    LocalTee(u32),
    GlobalGet(u32),
    GlobalSet(u32),
    I32Const(i32),
    F64Const(f64),
    I32Eqz,
    F64Eq,
    F64Ne,
    F64Lt,
    F64Gt,
    F64Le,
    F64Ge,
    F64Abs,
    F64Neg,
    F64Ceil,
    F64Floor,
    F64Trunc,
    F64Nearest,
    F64Sqrt,
    F64Add,
    F64Sub,
    F64Mul,
    F64Div,
    F64Min,
    F64Max,
    F64ConvertI32S,
}

#[derive(Clone, Debug)]
struct WasmParsedFunc {
    type_idx: u32,
    extra_locals: Vec<u8>,
    insns: Vec<WasmInsn>,
}

#[derive(Clone, Debug)]
struct WasmModule {
    types: Vec<WasmTypeSig>,
    imports: Vec<(String, String, u32)>,
    internal_funcs: Vec<WasmParsedFunc>,
    global_inits: Vec<f64>,
    exports: HashMap<String, u32>,
}

fn decode_wasm_expr(code: &[u8]) -> Result<Vec<WasmInsn>, String> {
    let mut pos = 0usize;
    let mut insns = Vec::new();
    enum Ctl {
        Block(usize),
        Loop(usize),
        If {
            if_idx: usize,
            else_idx: Option<usize>,
        },
    }
    let mut ctl: Vec<Ctl> = Vec::new();
    while pos < code.len() {
        let op = code[pos];
        pos += 1;
        let idx = insns.len();
        let item = match op {
            0x00 => WasmInsn::Unreachable,
            0x01 => WasmInsn::Nop,
            0x02 => {
                let bt = *code.get(pos).ok_or("wasm: block missing blocktype")?;
                pos += 1;
                ctl.push(Ctl::Block(idx));
                WasmInsn::Block {
                    block_type: bt,
                    end_idx: 0,
                }
            }
            0x03 => {
                let bt = *code.get(pos).ok_or("wasm: loop missing blocktype")?;
                pos += 1;
                ctl.push(Ctl::Loop(idx));
                WasmInsn::Loop {
                    block_type: bt,
                    end_idx: 0,
                }
            }
            0x04 => {
                let bt = *code.get(pos).ok_or("wasm: if missing blocktype")?;
                pos += 1;
                ctl.push(Ctl::If {
                    if_idx: idx,
                    else_idx: None,
                });
                WasmInsn::If {
                    block_type: bt,
                    else_idx: None,
                    end_idx: 0,
                }
            }
            0x05 => {
                match ctl.last_mut() {
                    Some(Ctl::If { if_idx, else_idx }) => {
                        *else_idx = Some(idx);
                        if let WasmInsn::If { else_idx: e, .. } = &mut insns[*if_idx] {
                            *e = Some(idx);
                        }
                    }
                    _ => return Err("wasm: else without matching if".into()),
                }
                WasmInsn::Else { end_idx: 0 }
            }
            0x0B => {
                if let Some(frame) = ctl.pop() {
                    match frame {
                        Ctl::Block(b) => {
                            if let WasmInsn::Block { end_idx, .. } = &mut insns[b] {
                                *end_idx = idx;
                            }
                        }
                        Ctl::Loop(l) => {
                            if let WasmInsn::Loop { end_idx, .. } = &mut insns[l] {
                                *end_idx = idx;
                            }
                        }
                        Ctl::If { if_idx, else_idx } => {
                            if let WasmInsn::If { end_idx, .. } = &mut insns[if_idx] {
                                *end_idx = idx;
                            }
                            if let Some(ei) = else_idx
                                && let WasmInsn::Else { end_idx } = &mut insns[ei]
                            {
                                *end_idx = idx;
                            }
                        }
                    }
                } else {
                    insns.push(WasmInsn::End);
                    break;
                }
                WasmInsn::End
            }
            0x0C => WasmInsn::Br(read_uleb128(code, &mut pos)?),
            0x0D => WasmInsn::BrIf(read_uleb128(code, &mut pos)?),
            0x0F => WasmInsn::Return,
            0x10 => WasmInsn::Call(read_uleb128(code, &mut pos)?),
            0x1A => WasmInsn::Drop,
            0x20 => WasmInsn::LocalGet(read_uleb128(code, &mut pos)?),
            0x21 => WasmInsn::LocalSet(read_uleb128(code, &mut pos)?),
            0x22 => WasmInsn::LocalTee(read_uleb128(code, &mut pos)?),
            0x23 => WasmInsn::GlobalGet(read_uleb128(code, &mut pos)?),
            0x24 => WasmInsn::GlobalSet(read_uleb128(code, &mut pos)?),
            0x41 => WasmInsn::I32Const(read_sleb128(code, &mut pos)? as i32),
            0x44 => {
                let raw: [u8; 8] = code
                    .get(pos..pos + 8)
                    .ok_or("wasm: EOF in f64.const")?
                    .try_into()
                    .map_err(|_| "wasm: slice")?;
                pos += 8;
                WasmInsn::F64Const(f64::from_le_bytes(raw))
            }
            0x45 => WasmInsn::I32Eqz,
            0x61 => WasmInsn::F64Eq,
            0x62 => WasmInsn::F64Ne,
            0x63 => WasmInsn::F64Lt,
            0x64 => WasmInsn::F64Gt,
            0x65 => WasmInsn::F64Le,
            0x66 => WasmInsn::F64Ge,
            0x99 => WasmInsn::F64Abs,
            0x9A => WasmInsn::F64Neg,
            0x9B => WasmInsn::F64Ceil,
            0x9C => WasmInsn::F64Floor,
            0x9D => WasmInsn::F64Trunc,
            0x9E => WasmInsn::F64Nearest,
            0x9F => WasmInsn::F64Sqrt,
            0xA0 => WasmInsn::F64Add,
            0xA1 => WasmInsn::F64Sub,
            0xA2 => WasmInsn::F64Mul,
            0xA3 => WasmInsn::F64Div,
            0xA4 => WasmInsn::F64Min,
            0xA5 => WasmInsn::F64Max,
            0xB7 => WasmInsn::F64ConvertI32S,
            other => return Err(format!("wasm: unsupported opcode 0x{other:02X}")),
        };
        insns.push(item);
    }
    Ok(insns)
}

fn parse_wasm_module(bytes: &[u8]) -> Result<WasmModule, String> {
    if bytes.len() < 8 || &bytes[0..4] != b"\0asm" {
        return Err("wasm: invalid magic header".into());
    }
    let ver = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    if ver != 1 {
        return Err(format!("wasm: unsupported version {ver}"));
    }

    let mut types = Vec::new();
    let mut imports = Vec::new();
    let mut fn_type_indices = Vec::new();
    let mut global_inits = Vec::new();
    let mut exports = HashMap::new();
    let mut internal_funcs = Vec::new();

    let mut pos = 8usize;
    while pos < bytes.len() {
        let sec_id = bytes[pos];
        pos += 1;
        let sec_len = read_uleb128(bytes, &mut pos)? as usize;
        let sec_end = pos + sec_len;
        if sec_end > bytes.len() {
            return Err("wasm: truncated section".into());
        }
        let sec = &bytes[pos..sec_end];
        let mut p = 0usize;
        match sec_id {
            1 => {
                let count = read_uleb128(sec, &mut p)?;
                for _ in 0..count {
                    let form = *sec.get(p).ok_or("wasm: EOF in type")?;
                    p += 1;
                    if form != 0x60 {
                        return Err(format!("wasm: unexpected type form 0x{form:02X}"));
                    }
                    let pc = read_uleb128(sec, &mut p)? as usize;
                    let params = sec.get(p..p + pc).ok_or("wasm: EOF params")?.to_vec();
                    p += pc;
                    let rc = read_uleb128(sec, &mut p)? as usize;
                    let results = sec.get(p..p + rc).ok_or("wasm: EOF results")?.to_vec();
                    p += rc;
                    types.push(WasmTypeSig { params, results });
                }
            }
            2 => {
                let count = read_uleb128(sec, &mut p)?;
                for _ in 0..count {
                    let module = read_wasm_name(sec, &mut p)?;
                    let field = read_wasm_name(sec, &mut p)?;
                    let kind = *sec.get(p).ok_or("wasm: EOF import kind")?;
                    p += 1;
                    if kind != 0x00 {
                        return Err(format!("wasm: unsupported import kind {kind}"));
                    }
                    let type_idx = read_uleb128(sec, &mut p)?;
                    imports.push((module, field, type_idx));
                }
            }
            3 => {
                let count = read_uleb128(sec, &mut p)?;
                for _ in 0..count {
                    fn_type_indices.push(read_uleb128(sec, &mut p)?);
                }
            }
            6 => {
                let count = read_uleb128(sec, &mut p)?;
                for _ in 0..count {
                    let val_type = *sec.get(p).ok_or("wasm: EOF global type")?;
                    let _mut = *sec.get(p + 1).ok_or("wasm: EOF global mut")?;
                    p += 2;
                    let op = *sec.get(p).ok_or("wasm: EOF global init")?;
                    p += 1;
                    let init_val = match (val_type, op) {
                        (WASM_VAL_F64, 0x44) => {
                            let raw: [u8; 8] = sec[p..p + 8].try_into().map_err(|_| "wasm: f64")?;
                            p += 8;
                            f64::from_le_bytes(raw)
                        }
                        (_, 0x41) => read_sleb128(sec, &mut p)? as f64,
                        _ => return Err("wasm: unsupported global init expr".into()),
                    };
                    if *sec.get(p).unwrap_or(&0) != 0x0B {
                        return Err("wasm: missing global init end".into());
                    }
                    p += 1;
                    global_inits.push(init_val);
                }
            }
            7 => {
                let count = read_uleb128(sec, &mut p)?;
                for _ in 0..count {
                    let name = read_wasm_name(sec, &mut p)?;
                    let kind = *sec.get(p).ok_or("wasm: EOF export kind")?;
                    p += 1;
                    let index = read_uleb128(sec, &mut p)?;
                    if kind == 0x00 {
                        exports.insert(name, index);
                    }
                }
            }
            10 => {
                let count = read_uleb128(sec, &mut p)?;
                if count as usize != fn_type_indices.len() {
                    return Err("wasm: function/code count mismatch".into());
                }
                for i in 0..count as usize {
                    let body_size = read_uleb128(sec, &mut p)? as usize;
                    let body = sec.get(p..p + body_size).ok_or("wasm: EOF code body")?;
                    p += body_size;
                    let mut bp = 0usize;
                    let decls = read_uleb128(body, &mut bp)?;
                    let mut extra_locals = Vec::new();
                    for _ in 0..decls {
                        let lc = read_uleb128(body, &mut bp)? as usize;
                        let lt = *body.get(bp).ok_or("wasm: EOF local type")?;
                        bp += 1;
                        for _ in 0..lc {
                            extra_locals.push(lt);
                        }
                    }
                    let insns = decode_wasm_expr(&body[bp..])?;
                    internal_funcs.push(WasmParsedFunc {
                        type_idx: fn_type_indices[i],
                        extra_locals,
                        insns,
                    });
                }
            }
            0 | 4 | 5 | 8 | 9 | 11 => {}
            other => return Err(format!("wasm: unknown section {other}")),
        }
        pos = sec_end;
    }

    Ok(WasmModule {
        types,
        imports,
        internal_funcs,
        global_inits,
        exports,
    })
}

pub(crate) fn wasm_disasm(bytes: &[u8]) -> Result<String, String> {
    let m = parse_wasm_module(bytes)?;
    let mut out = String::from("(module\n");
    for (i, t) in m.types.iter().enumerate() {
        out.push_str(&format!(
            "  (type (;{i};) (func (params {}) (results {})))\n",
            t.params.len(),
            t.results.len()
        ));
    }
    for (i, (mod_n, field_n, tid)) in m.imports.iter().enumerate() {
        out.push_str(&format!(
            "  (import \"{mod_n}\" \"{field_n}\" (func (;{i};) (type {tid})))\n"
        ));
    }
    for (name, idx) in &m.exports {
        out.push_str(&format!("  (export \"{name}\" (func {idx}))\n"));
    }
    for (i, f) in m.internal_funcs.iter().enumerate() {
        let fid = m.imports.len() + i;
        out.push_str(&format!(
            "  (func (;{fid};) (type {}) (locals {})\n",
            f.type_idx,
            f.extra_locals.len()
        ));
        for insn in &f.insns {
            out.push_str(&format!("    {insn:?}\n"));
        }
        out.push_str("  )\n");
    }
    out.push(')');
    Ok(out)
}

#[derive(Clone, Copy)]
enum WasmVal {
    I32(i32),
    F64(f64),
}

impl WasmVal {
    fn as_f64(self) -> Result<f64, String> {
        match self {
            WasmVal::F64(x) => Ok(x),
            WasmVal::I32(x) => Ok(f64::from(x)),
        }
    }
    fn as_i32(self) -> Result<i32, String> {
        match self {
            WasmVal::I32(x) => Ok(x),
            WasmVal::F64(x) => Ok(if x != 0.0 { 1 } else { 0 }),
        }
    }
}

pub(crate) fn wasm_run(
    bytes: &[u8],
    export_name: &str,
    args: &[f64],
) -> Result<(f64, Vec<String>), String> {
    let m = parse_wasm_module(bytes)?;
    let fn_idx = *m
        .exports
        .get(export_name)
        .ok_or_else(|| format!("wasm: export '{export_name}' not found"))?;
    let mut globals = m.global_inits.clone();
    let mut prints = Vec::new();
    let mut steps = 0usize;

    fn exec_fn(
        m: &WasmModule,
        fn_idx: u32,
        args: &[WasmVal],
        globals: &mut Vec<f64>,
        prints: &mut Vec<String>,
        steps: &mut usize,
    ) -> Result<Option<WasmVal>, String> {
        let imp_count = m.imports.len() as u32;
        if fn_idx < imp_count {
            let (mod_n, field_n, _) = &m.imports[fn_idx as usize];
            if mod_n == "env" && field_n == "print_num" {
                let val = args
                    .first()
                    .copied()
                    .unwrap_or(WasmVal::F64(0.0))
                    .as_f64()?;
                let formatted = if val.fract() == 0.0 && val.abs() < 9.0e15 {
                    format!("{}", val as i64)
                } else {
                    format!("{val}")
                };
                println!("{formatted}");
                prints.push(formatted);
                return Ok(None);
            }
            return Err(format!("wasm: unknown host import {mod_n}.{field_n}"));
        }

        let func = m
            .internal_funcs
            .get((fn_idx - imp_count) as usize)
            .ok_or("wasm: function index out of bounds")?;
        let sig = m
            .types
            .get(func.type_idx as usize)
            .ok_or("wasm: type index out of bounds")?;
        if sig.params.len() != args.len() {
            return Err(format!(
                "wasm: expected {} args, got {}",
                sig.params.len(),
                args.len()
            ));
        }

        let mut locals = Vec::with_capacity(sig.params.len() + func.extra_locals.len());
        locals.extend_from_slice(args);
        for &lt in &func.extra_locals {
            locals.push(if lt == WASM_VAL_I32 {
                WasmVal::I32(0)
            } else {
                WasmVal::F64(0.0)
            });
        }

        #[derive(Clone, Copy)]
        struct Label {
            target_ip: usize,
            stack_height: usize,
            is_loop: bool,
        }
        let mut labels: Vec<Label> = Vec::new();
        let mut stack: Vec<WasmVal> = Vec::new();
        let mut ip = 0usize;

        while ip < func.insns.len() {
            *steps += 1;
            if *steps > 20_000_000 {
                return Err("wasm: execution step limit exceeded".into());
            }
            let insn = &func.insns[ip];
            match insn {
                WasmInsn::Unreachable => return Err("wasm trap: unreachable".into()),
                WasmInsn::Nop => ip += 1,
                WasmInsn::Block {
                    block_type,
                    end_idx,
                } => {
                    let res_arity = usize::from(*block_type != WASM_VOID);
                    labels.push(Label {
                        target_ip: *end_idx + 1,
                        stack_height: stack.len() + res_arity,
                        is_loop: false,
                    });
                    ip += 1;
                }
                WasmInsn::Loop {
                    block_type,
                    end_idx,
                } => {
                    let _ = (*block_type, end_idx);
                    labels.push(Label {
                        target_ip: ip + 1,
                        stack_height: stack.len(),
                        is_loop: true,
                    });
                    ip += 1;
                }
                WasmInsn::If {
                    block_type,
                    else_idx,
                    end_idx,
                } => {
                    let cond = stack.pop().ok_or("wasm: if stack underflow")?.as_i32()?;
                    let res_arity = usize::from(*block_type != WASM_VOID);
                    labels.push(Label {
                        target_ip: *end_idx + 1,
                        stack_height: stack.len() + res_arity,
                        is_loop: false,
                    });
                    if cond != 0 {
                        ip += 1;
                    } else if let Some(ei) = else_idx {
                        ip = *ei + 1;
                    } else {
                        labels.pop();
                        ip = *end_idx + 1;
                    }
                }
                WasmInsn::Else { end_idx } => {
                    labels.pop();
                    ip = *end_idx + 1;
                }
                WasmInsn::End => {
                    if labels.pop().is_none() {
                        break;
                    }
                    ip += 1;
                }
                WasmInsn::Br(depth) | WasmInsn::BrIf(depth) => {
                    let take = match insn {
                        WasmInsn::Br(_) => true,
                        WasmInsn::BrIf(_) => {
                            stack.pop().ok_or("wasm: br_if underflow")?.as_i32()? != 0
                        }
                        _ => unreachable!(),
                    };
                    if take {
                        let d = *depth as usize;
                        if d >= labels.len() {
                            return Ok(stack.pop());
                        }
                        let target_pos = labels.len() - 1 - d;
                        let lbl = labels[target_pos];
                        if lbl.is_loop {
                            labels.truncate(target_pos + 1);
                            stack.truncate(lbl.stack_height);
                        } else {
                            labels.truncate(target_pos);
                            stack.truncate(lbl.stack_height);
                        }
                        ip = lbl.target_ip;
                    } else {
                        ip += 1;
                    }
                }
                WasmInsn::Return => {
                    return Ok(if sig.results.is_empty() {
                        None
                    } else {
                        Some(stack.pop().ok_or("wasm: missing return value")?)
                    });
                }
                WasmInsn::Call(callee) => {
                    let c_tid = if (*callee as usize) < m.imports.len() {
                        m.imports[*callee as usize].2
                    } else {
                        m.internal_funcs[*callee as usize - m.imports.len()].type_idx
                    };
                    let csig = &m.types[c_tid as usize];
                    let ac = csig.params.len();
                    if stack.len() < ac {
                        return Err("wasm: call underflow".into());
                    }
                    let cargs = stack.split_off(stack.len() - ac);
                    if let Some(rv) = exec_fn(m, *callee, &cargs, globals, prints, steps)? {
                        stack.push(rv);
                    }
                    ip += 1;
                }
                WasmInsn::Drop => {
                    stack.pop().ok_or("wasm: drop underflow")?;
                    ip += 1;
                }
                WasmInsn::LocalGet(i) => {
                    stack.push(locals[*i as usize]);
                    ip += 1;
                }
                WasmInsn::LocalSet(i) => {
                    locals[*i as usize] = stack.pop().ok_or("wasm: local.set underflow")?;
                    ip += 1;
                }
                WasmInsn::LocalTee(i) => {
                    let v = *stack.last().ok_or("wasm: local.tee underflow")?;
                    locals[*i as usize] = v;
                    ip += 1;
                }
                WasmInsn::GlobalGet(i) => {
                    stack.push(WasmVal::F64(globals[*i as usize]));
                    ip += 1;
                }
                WasmInsn::GlobalSet(i) => {
                    globals[*i as usize] =
                        stack.pop().ok_or("wasm: global.set underflow")?.as_f64()?;
                    ip += 1;
                }
                WasmInsn::I32Const(v) => {
                    stack.push(WasmVal::I32(*v));
                    ip += 1;
                }
                WasmInsn::F64Const(v) => {
                    stack.push(WasmVal::F64(*v));
                    ip += 1;
                }
                WasmInsn::I32Eqz => {
                    let x = stack.pop().ok_or("wasm: underflow")?.as_i32()?;
                    stack.push(WasmVal::I32(i32::from(x == 0)));
                    ip += 1;
                }
                WasmInsn::F64ConvertI32S => {
                    let x = stack.pop().ok_or("wasm: underflow")?.as_i32()?;
                    stack.push(WasmVal::F64(f64::from(x)));
                    ip += 1;
                }
                WasmInsn::F64Abs
                | WasmInsn::F64Neg
                | WasmInsn::F64Ceil
                | WasmInsn::F64Floor
                | WasmInsn::F64Trunc
                | WasmInsn::F64Nearest
                | WasmInsn::F64Sqrt => {
                    let x = stack.pop().ok_or("wasm: underflow")?.as_f64()?;
                    let res = match insn {
                        WasmInsn::F64Abs => x.abs(),
                        WasmInsn::F64Neg => -x,
                        WasmInsn::F64Ceil => x.ceil(),
                        WasmInsn::F64Floor => x.floor(),
                        WasmInsn::F64Trunc => x.trunc(),
                        WasmInsn::F64Nearest => x.round(),
                        WasmInsn::F64Sqrt => x.sqrt(),
                        _ => unreachable!(),
                    };
                    stack.push(WasmVal::F64(res));
                    ip += 1;
                }
                WasmInsn::F64Add
                | WasmInsn::F64Sub
                | WasmInsn::F64Mul
                | WasmInsn::F64Div
                | WasmInsn::F64Min
                | WasmInsn::F64Max => {
                    let b = stack.pop().ok_or("wasm: underflow")?.as_f64()?;
                    let a = stack.pop().ok_or("wasm: underflow")?.as_f64()?;
                    let res = match insn {
                        WasmInsn::F64Add => a + b,
                        WasmInsn::F64Sub => a - b,
                        WasmInsn::F64Mul => a * b,
                        WasmInsn::F64Div => a / b,
                        WasmInsn::F64Min => a.min(b),
                        WasmInsn::F64Max => a.max(b),
                        _ => unreachable!(),
                    };
                    stack.push(WasmVal::F64(res));
                    ip += 1;
                }
                WasmInsn::F64Eq
                | WasmInsn::F64Ne
                | WasmInsn::F64Lt
                | WasmInsn::F64Gt
                | WasmInsn::F64Le
                | WasmInsn::F64Ge => {
                    let b = stack.pop().ok_or("wasm: underflow")?.as_f64()?;
                    let a = stack.pop().ok_or("wasm: underflow")?.as_f64()?;
                    let ok = match insn {
                        WasmInsn::F64Eq => a == b,
                        WasmInsn::F64Ne => a != b,
                        WasmInsn::F64Lt => a < b,
                        WasmInsn::F64Gt => a > b,
                        WasmInsn::F64Le => a <= b,
                        WasmInsn::F64Ge => a >= b,
                        _ => unreachable!(),
                    };
                    stack.push(WasmVal::I32(i32::from(ok)));
                    ip += 1;
                }
            }
        }
        Ok(if sig.results.is_empty() {
            None
        } else {
            Some(stack.pop().unwrap_or(WasmVal::F64(0.0)))
        })
    }

    let wargs: Vec<WasmVal> = args.iter().map(|&x| WasmVal::F64(x)).collect();
    let res = exec_fn(&m, fn_idx, &wargs, &mut globals, &mut prints, &mut steps)?
        .map(|v| v.as_f64())
        .transpose()?
        .unwrap_or(0.0);
    Ok((res, prints))
}

// ============================================================================
// Part B: Direct Zero-Toolchain Native x86_64 PE32+ / ELF64 Emitter
// ============================================================================

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum DirectNativeFormat {
    Pe64,
    Elf64,
}

impl DirectNativeFormat {
    pub(crate) fn for_path(out: &Path) -> Self {
        if out
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("exe"))
            || cfg!(target_os = "windows")
                && !out
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e.eq_ignore_ascii_case("elf"))
        {
            DirectNativeFormat::Pe64
        } else {
            DirectNativeFormat::Elf64
        }
    }
}

struct X64Asm {
    code: Vec<u8>,
    iat_fixups: Vec<(usize, usize)>, // (disp32_offset_in_code, iat_entry_index: 0=ExitProcess, 1=GetStdHandle, 2=WriteFile)
    call_fixups: Vec<(usize, String)>,
    fn_offsets: HashMap<String, usize>,
    globals: HashMap<String, usize>, // var -> fixed global index (in frame of entry or unused if in main)
}

impl X64Asm {
    fn new() -> Self {
        Self {
            code: Vec::new(),
            iat_fixups: Vec::new(),
            call_fixups: Vec::new(),
            fn_offsets: HashMap::new(),
            globals: HashMap::new(),
        }
    }

    fn emit_i32(&mut self, v: i32) {
        self.code.extend_from_slice(&v.to_le_bytes());
    }
    fn emit_u64(&mut self, v: u64) {
        self.code.extend_from_slice(&v.to_le_bytes());
    }

    fn emit_mov_rax_imm64(&mut self, v: i64) {
        self.code.extend_from_slice(&[0x48, 0xB8]);
        self.emit_u64(v as u64);
    }

    fn emit_push_rax(&mut self) {
        self.code.push(0x50);
    }
    fn emit_pop_rax(&mut self) {
        self.code.push(0x58);
    }

    fn emit_call_rel32(&mut self, name: &str) {
        self.code.push(0xE8);
        let patch = self.code.len();
        self.emit_i32(0);
        self.call_fixups.push((patch, name.to_string()));
    }

    fn emit_call_iat(&mut self, iat_idx: usize) {
        self.code.extend_from_slice(&[0xFF, 0x15]);
        let patch = self.code.len();
        self.emit_i32(0);
        self.iat_fixups.push((patch, iat_idx));
    }

    fn emit_jmp_placeholder(&mut self) -> usize {
        self.code.push(0xE9);
        let p = self.code.len();
        self.emit_i32(0);
        p
    }

    fn emit_jz_placeholder(&mut self) -> usize {
        self.code.extend_from_slice(&[0x48, 0x85, 0xC0, 0x0F, 0x84]); // test rax,rax; jz rel32
        let p = self.code.len();
        self.emit_i32(0);
        p
    }

    fn patch_rel32(&mut self, disp_off: usize, target_off: usize) {
        let rel = (target_off as isize - (disp_off as isize + 4)) as i32;
        self.code[disp_off..disp_off + 4].copy_from_slice(&rel.to_le_bytes());
    }

    fn emit_load_rbp_slot(&mut self, disp: i32) {
        self.code.extend_from_slice(&[0x48, 0x8B, 0x85]); // mov rax, [rbp + disp32]
        self.emit_i32(disp);
    }

    fn emit_store_rbp_slot(&mut self, disp: i32) {
        self.code.extend_from_slice(&[0x48, 0x89, 0x85]); // mov [rbp + disp32], rax
        self.emit_i32(disp);
    }

    fn emit_runtime_stubs(&mut self, fmt: DirectNativeFormat) {
        // __rt_exit: ecx = exit code
        self.fn_offsets
            .insert("__rt_exit".to_string(), self.code.len());
        match fmt {
            DirectNativeFormat::Pe64 => {
                self.code.extend_from_slice(&[0x48, 0x83, 0xE4, 0xF0]); // and rsp, -16
                self.code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x20]); // sub rsp, 32
                self.emit_call_iat(0); // ExitProcess(ecx)
                self.code.push(0xCC);
            }
            DirectNativeFormat::Elf64 => {
                self.code.extend_from_slice(&[0x48, 0x89, 0xCF]); // mov rdi, rcx
                self.code.extend_from_slice(&[0xB8, 60, 0, 0, 0]); // mov eax, 60
                self.code.extend_from_slice(&[0x0F, 0x05]); // syscall
                self.code.push(0xCC);
            }
        }

        // __rt_print_i64: rax = signed integer to print followed by '\n'
        self.fn_offsets
            .insert("__rt_print_i64".to_string(), self.code.len());
        self.code.push(0x55); // push rbp
        self.code.extend_from_slice(&[0x48, 0x89, 0xE5]); // mov rbp, rsp
        self.code.push(0x53); // push rbx
        self.code.extend_from_slice(&[0x41, 0x54]); // push r12
        self.code.extend_from_slice(&[0x41, 0x55]); // push r13
        self.code.extend_from_slice(&[0x41, 0x56]); // push r14
        // Use r14 as negative flag, r12 as buf end/start cursor, rbx = divisor 10
        // Allocate aligned buffer on stack: sub rsp, 96; and rsp, -16
        self.code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x60]);
        self.code.extend_from_slice(&[0x48, 0x83, 0xE4, 0xF0]);
        // r12 = rsp + 80 (write digits backwards from [r12], r13 = end)
        self.code.extend_from_slice(&[0x4C, 0x8D, 0x64, 0x24, 0x50]); // lea r12, [rsp+0x50]
        self.code.extend_from_slice(&[0x4D, 0x89, 0xE5]); // mov r13, r12
        self.code.extend_from_slice(&[0x41, 0xC6, 0x04, 0x24, 0x0A]); // mov byte [r12], '\n'
        self.code.extend_from_slice(&[0x45, 0x31, 0xF6]); // xor r14d, r14d (is_neg = 0)
        // test rax, rax; jge +non_neg
        self.code.extend_from_slice(&[0x48, 0x85, 0xC0, 0x7D, 0x06]);
        self.code.extend_from_slice(&[0x48, 0xF7, 0xD8]); // neg rax
        self.code.extend_from_slice(&[0x41, 0xFE, 0xC6]); // inc r14b
        // rbx = 10
        self.code.extend_from_slice(&[0xBB, 10, 0, 0, 0]);
        let loop_start = self.code.len();
        self.code.extend_from_slice(&[0x49, 0xFF, 0xCC]); // dec r12
        self.code.extend_from_slice(&[0x31, 0xD2]); // xor edx, edx
        self.code.extend_from_slice(&[0x48, 0xF7, 0xF3]); // div rbx (unsigned works for all <= i64::MAX!)
        self.code.extend_from_slice(&[0x80, 0xC2, b'0']); // add dl, '0'
        self.code.extend_from_slice(&[0x41, 0x88, 0x14, 0x24]); // mov [r12], dl
        self.code.extend_from_slice(&[0x48, 0x85, 0xC0]); // test rax, rax
        let rel_loop = (loop_start as isize - (self.code.len() as isize + 2)) as i8;
        self.code.extend_from_slice(&[0x75, rel_loop as u8]); // jnz loop_start

        // if is_neg != 0: dec r12; mov [r12], '-' (3 + 5 = 8 bytes skipped if r14b == 0)
        self.code.extend_from_slice(&[0x45, 0x84, 0xF6, 0x74, 0x08]); // test r14b,r14b; jz +8
        self.code.extend_from_slice(&[0x49, 0xFF, 0xCC]); // dec r12
        self.code.extend_from_slice(&[0x41, 0xC6, 0x04, 0x24, b'-']);

        // length = (r13 - r12) + 1 -> r13
        self.code.extend_from_slice(&[0x4D, 0x29, 0xE5]); // sub r13, r12
        self.code.extend_from_slice(&[0x49, 0xFF, 0xC5]); // inc r13

        match fmt {
            DirectNativeFormat::Pe64 => {
                // rsp is currently 16-byte aligned; allocate 48 bytes (32 shadow + 8 arg5 + 8 written)
                // Keep written at [rsp + 0x28], arg5 at [rsp + 0x20]
                self.code.extend_from_slice(&[0xB9, 0xF5, 0xFF, 0xFF, 0xFF]); // mov ecx, -11 (STD_OUTPUT_HANDLE)
                self.emit_call_iat(1); // GetStdHandle
                self.code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax
                self.code.extend_from_slice(&[0x4C, 0x89, 0xE2]); // mov rdx, r12
                self.code.extend_from_slice(&[0x4D, 0x89, 0xE8]); // mov r8, r13
                self.code.extend_from_slice(&[0x4C, 0x8D, 0x4C, 0x24, 0x28]); // lea r9, [rsp+0x28]
                self.code
                    .extend_from_slice(&[0x48, 0xC7, 0x44, 0x24, 0x20, 0, 0, 0, 0]); // mov qword [rsp+0x20], 0
                self.emit_call_iat(2); // WriteFile
            }
            DirectNativeFormat::Elf64 => {
                self.code.extend_from_slice(&[0xB8, 1, 0, 0, 0]); // sys_write
                self.code.extend_from_slice(&[0xBF, 1, 0, 0, 0]); // fd = 1
                self.code.extend_from_slice(&[0x4C, 0x89, 0xE6]); // mov rsi, r12
                self.code.extend_from_slice(&[0x4C, 0x89, 0xEA]); // mov rdx, r13
                self.code.extend_from_slice(&[0x0F, 0x05]); // syscall
            }
        }

        // Epilogue (restore callee-saved regs using rbp - 32!)
        self.code.extend_from_slice(&[0x48, 0x8D, 0x65, 0xE0]); // lea rsp, [rbp - 32]
        self.code.extend_from_slice(&[0x41, 0x5E]); // pop r14
        self.code.extend_from_slice(&[0x41, 0x5D]); // pop r13
        self.code.extend_from_slice(&[0x41, 0x5C]); // pop r12
        self.code.push(0x5B); // pop rbx
        self.code.push(0x5D); // pop rbp
        self.code.push(0xC3); // ret
    }
}

struct X64FnCtx<'a> {
    asm: &'a mut X64Asm,
    vars: HashMap<String, i32>, // local name -> [rbp + disp]
    next_local_slot: i32,
    loop_stack: Vec<(Vec<usize>, usize)>, // (break_fixups, continue_target)
    epilogue_jumps: Vec<usize>,
}

impl<'a> X64FnCtx<'a> {
    fn get_or_alloc_local(&mut self, name: &str) -> i32 {
        if let Some(&disp) = self.vars.get(name) {
            disp
        } else {
            self.next_local_slot += 1;
            let disp = -self.next_local_slot * 8;
            self.vars.insert(name.to_string(), disp);
            disp
        }
    }

    fn alloc_temp(&mut self) -> i32 {
        self.next_local_slot += 1;
        -self.next_local_slot * 8
    }

    fn compile_expr(&mut self, e: &Expr) -> Result<(), String> {
        match e {
            Expr::Num(n) => self.asm.emit_mov_rax_imm64(*n as i64),
            Expr::Bool(b) => self.asm.emit_mov_rax_imm64(i64::from(*b)),
            Expr::Null => self.asm.code.extend_from_slice(&[0x31, 0xC0]), // xor eax, eax
            Expr::Var(name) => {
                if let Some(&disp) = self.vars.get(name) {
                    self.asm.emit_load_rbp_slot(disp);
                } else if let Some(&gdisp) = self.asm.globals.get(name) {
                    self.asm.emit_load_rbp_slot(gdisp as i32);
                } else {
                    return Err(format!("direct-aot: undefined variable '{name}'"));
                }
            }
            Expr::Unary(op, inner) => {
                self.compile_expr(inner)?;
                match op.as_str() {
                    "-" => self.asm.code.extend_from_slice(&[0x48, 0xF7, 0xD8]), // neg rax
                    "+" => {}
                    "not" | "!" => {
                        self.asm.code.extend_from_slice(&[
                            0x48, 0x85, 0xC0, 0x0F, 0x94, 0xC0, 0x48, 0x0F, 0xB6, 0xC0,
                        ]);
                    }
                    _ => return Err(format!("direct-aot: unsupported unary '{op}'")),
                }
            }
            Expr::Bin(l, op, r) => match op.as_str() {
                "and" => {
                    self.compile_expr(l)?;
                    let skip = self.asm.emit_jz_placeholder();
                    self.compile_expr(r)?;
                    let end = self.asm.code.len();
                    self.asm.patch_rel32(skip, end);
                }
                "or" => {
                    self.compile_expr(l)?;
                    self.asm
                        .code
                        .extend_from_slice(&[0x48, 0x85, 0xC0, 0x0F, 0x85]); // jnz end
                    let skip = self.asm.code.len();
                    self.asm.emit_i32(0);
                    self.compile_expr(r)?;
                    let end = self.asm.code.len();
                    self.asm.patch_rel32(skip, end);
                }
                _ => {
                    self.compile_expr(l)?;
                    self.asm.emit_push_rax();
                    self.compile_expr(r)?;
                    self.asm.code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax (rhs)
                    self.asm.emit_pop_rax(); // rax = lhs
                    match op.as_str() {
                        "+" => self.asm.code.extend_from_slice(&[0x48, 0x01, 0xC8]), // add rax, rcx
                        "-" => self.asm.code.extend_from_slice(&[0x48, 0x29, 0xC8]), // sub rax, rcx
                        "*" => self.asm.code.extend_from_slice(&[0x48, 0x0F, 0xAF, 0xC1]), // imul rax, rcx
                        "/" => {
                            self.asm.code.push(0x48);
                            self.asm.code.push(0x99); // cqo
                            self.asm.code.extend_from_slice(&[0x48, 0xF7, 0xF9]); // idiv rcx
                        }
                        "%" => {
                            self.asm.code.push(0x48);
                            self.asm.code.push(0x99); // cqo
                            self.asm.code.extend_from_slice(&[0x48, 0xF7, 0xF9]); // idiv rcx
                            self.asm.code.extend_from_slice(&[0x48, 0x89, 0xD0]); // mov rax, rdx
                        }
                        "==" | "!=" | "<" | "<=" | ">" | ">=" => {
                            self.asm.code.extend_from_slice(&[0x48, 0x39, 0xC8]); // cmp rax, rcx
                            let cc = match op.as_str() {
                                "==" => 0x94,
                                "!=" => 0x95,
                                "<" => 0x9C,
                                ">=" => 0x9D,
                                "<=" => 0x9E,
                                ">" => 0x9F,
                                _ => unreachable!(),
                            };
                            self.asm
                                .code
                                .extend_from_slice(&[0x0F, cc, 0xC0, 0x48, 0x0F, 0xB6, 0xC0]);
                        }
                        _ => return Err(format!("direct-aot: unsupported binary op '{op}'")),
                    }
                }
            },
            Expr::Ternary(c, t, f) => {
                self.compile_expr(c)?;
                let jz = self.asm.emit_jz_placeholder();
                self.compile_expr(t)?;
                let jmp = self.asm.emit_jmp_placeholder();
                let f_off = self.asm.code.len();
                self.asm.patch_rel32(jz, f_off);
                self.compile_expr(f)?;
                let end = self.asm.code.len();
                self.asm.patch_rel32(jmp, end);
            }
            Expr::Call(name, args) => match name.as_str() {
                "print" | "println" => {
                    for a in args {
                        self.compile_expr(a)?;
                        self.asm.emit_call_rel32("__rt_print_i64");
                    }
                    self.asm.code.extend_from_slice(&[0x31, 0xC0]);
                }
                "abs" if args.len() == 1 => {
                    self.compile_expr(&args[0])?;
                    self.asm
                        .code
                        .extend_from_slice(&[0x48, 0x99, 0x48, 0x31, 0xD0, 0x48, 0x29, 0xD0]); // cqo; xor rax, rdx; sub rax, rdx
                }
                "int" | "num" if args.len() == 1 => {
                    self.compile_expr(&args[0])?;
                }
                "requires" | "ensures" | "assert" => {
                    if let Some(cond) = args.first() {
                        self.compile_expr(cond)?;
                        let skip = self.asm.emit_jz_placeholder();
                        let ok = self.asm.emit_jmp_placeholder();
                        let fail_off = self.asm.code.len();
                        self.asm.patch_rel32(skip, fail_off);
                        self.asm.code.extend_from_slice(&[0xB9, 1, 0, 0, 0]);
                        self.asm.emit_call_rel32("__rt_exit");
                        let end = self.asm.code.len();
                        self.asm.patch_rel32(ok, end);
                    }
                    self.asm.code.extend_from_slice(&[0x31, 0xC0]);
                }
                _ => {
                    // Push args in reverse order so arg0 lands at [rbp+16], arg1 at [rbp+24]...
                    for a in args.iter().rev() {
                        self.compile_expr(a)?;
                        self.asm.emit_push_rax();
                    }
                    self.asm.emit_call_rel32(name);
                    if !args.is_empty() {
                        self.asm.code.extend_from_slice(&[0x48, 0x81, 0xC4]); // add rsp, imm32
                        self.asm.emit_i32((args.len() * 8) as i32);
                    }
                }
            },
            _ => return Err(format!("direct-aot: unsupported expression '{e:?}'")),
        }
        Ok(())
    }

    fn compile_stmt(&mut self, s: &Stmt) -> Result<(), String> {
        match s {
            Stmt::SourceLine(_) | Stmt::Fn(..) | Stmt::AsyncFn(..) | Stmt::DecoratedFn(..) => {}
            Stmt::Let(name, _, val, _) => {
                self.compile_expr(val)?;
                let disp = self.get_or_alloc_local(name);
                self.asm.emit_store_rbp_slot(disp);
            }
            Stmt::Assign(name, val) => {
                self.compile_expr(val)?;
                let disp = *self
                    .vars
                    .get(name)
                    .ok_or_else(|| format!("direct-aot: undefined '{name}'"))?;
                self.asm.emit_store_rbp_slot(disp);
            }
            Stmt::Expr(e) => {
                self.compile_expr(e)?;
            }
            Stmt::If(c, t, f) => {
                self.compile_expr(c)?;
                let jz = self.asm.emit_jz_placeholder();
                for x in t {
                    self.compile_stmt(x)?;
                }
                if f.is_empty() {
                    let end = self.asm.code.len();
                    self.asm.patch_rel32(jz, end);
                } else {
                    let jmp = self.asm.emit_jmp_placeholder();
                    let else_off = self.asm.code.len();
                    self.asm.patch_rel32(jz, else_off);
                    for x in f {
                        self.compile_stmt(x)?;
                    }
                    let end = self.asm.code.len();
                    self.asm.patch_rel32(jmp, end);
                }
            }
            Stmt::While(c, b) => {
                let top = self.asm.code.len();
                self.compile_expr(c)?;
                let jz = self.asm.emit_jz_placeholder();
                self.loop_stack.push((vec![jz], top));
                for x in b {
                    self.compile_stmt(x)?;
                }
                let jmp = self.asm.emit_jmp_placeholder();
                self.asm.patch_rel32(jmp, top);
                let end = self.asm.code.len();
                if let Some((breaks, _)) = self.loop_stack.pop() {
                    for br in breaks {
                        self.asm.patch_rel32(br, end);
                    }
                }
            }
            Stmt::ForRange(var, start, end_e, body) => {
                let v_disp = self.get_or_alloc_local(var);
                let e_disp = self.alloc_temp();
                self.compile_expr(start)?;
                self.asm.emit_store_rbp_slot(v_disp);
                self.compile_expr(end_e)?;
                self.asm.emit_store_rbp_slot(e_disp);

                let cond_off = self.asm.code.len();
                self.asm.emit_load_rbp_slot(e_disp);
                self.asm.code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax
                self.asm.emit_load_rbp_slot(v_disp);
                self.asm.code.extend_from_slice(&[0x48, 0x39, 0xC8]); // cmp rax, rcx
                self.asm.code.extend_from_slice(&[0x0F, 0x8D]); // jge break
                let jge = self.asm.code.len();
                self.asm.emit_i32(0);

                // Continue jumps to increment step, NOT directly to cond_off
                let step_jump_slot = self.loop_stack.len();
                let mut cont_jumps: Vec<usize> = Vec::new();
                self.loop_stack.push((vec![jge], usize::MAX));
                for x in body {
                    // Handle continue/break cleanly via patched step target
                    self.compile_stmt_for_loop(x, step_jump_slot, &mut cont_jumps)?;
                }
                let step_off = self.asm.code.len();
                for cj in cont_jumps {
                    self.asm.patch_rel32(cj, step_off);
                }
                self.asm.emit_load_rbp_slot(v_disp);
                self.asm.code.extend_from_slice(&[0x48, 0xFF, 0xC0]); // inc rax
                self.asm.emit_store_rbp_slot(v_disp);
                let back = self.asm.emit_jmp_placeholder();
                self.asm.patch_rel32(back, cond_off);
                let end = self.asm.code.len();
                if let Some((breaks, _)) = self.loop_stack.pop() {
                    for br in breaks {
                        self.asm.patch_rel32(br, end);
                    }
                }
            }
            Stmt::Break => {
                let jmp = self.asm.emit_jmp_placeholder();
                let top = self
                    .loop_stack
                    .last_mut()
                    .ok_or("direct-aot: break outside loop")?;
                top.0.push(jmp);
            }
            Stmt::Continue => {
                let top = self
                    .loop_stack
                    .last()
                    .ok_or("direct-aot: continue outside loop")?;
                let target = top.1;
                if target == usize::MAX {
                    return Err("direct-aot: continue inside for-range internal error".into());
                }
                let jmp = self.asm.emit_jmp_placeholder();
                self.asm.patch_rel32(jmp, target);
            }
            Stmt::Return(Some(e)) => {
                self.compile_expr(e)?;
                let jmp = self.asm.emit_jmp_placeholder();
                self.epilogue_jumps.push(jmp);
            }
            Stmt::Return(None) => {
                self.asm.code.extend_from_slice(&[0x31, 0xC0]);
                let jmp = self.asm.emit_jmp_placeholder();
                self.epilogue_jumps.push(jmp);
            }
            _ => return Err(format!("direct-aot: unsupported statement '{s:?}'")),
        }
        Ok(())
    }

    fn compile_stmt_for_loop(
        &mut self,
        s: &Stmt,
        loop_idx: usize,
        cont_jumps: &mut Vec<usize>,
    ) -> Result<(), String> {
        if self.loop_stack.len() - 1 == loop_idx && matches!(s, Stmt::Continue) {
            let jmp = self.asm.emit_jmp_placeholder();
            cont_jumps.push(jmp);
            return Ok(());
        }
        match s {
            Stmt::If(c, t, f) => {
                self.compile_expr(c)?;
                let jz = self.asm.emit_jz_placeholder();
                for x in t {
                    self.compile_stmt_for_loop(x, loop_idx, cont_jumps)?;
                }
                if f.is_empty() {
                    let end = self.asm.code.len();
                    self.asm.patch_rel32(jz, end);
                } else {
                    let jmp = self.asm.emit_jmp_placeholder();
                    let else_off = self.asm.code.len();
                    self.asm.patch_rel32(jz, else_off);
                    for x in f {
                        self.compile_stmt_for_loop(x, loop_idx, cont_jumps)?;
                    }
                    let end = self.asm.code.len();
                    self.asm.patch_rel32(jmp, end);
                }
                Ok(())
            }
            _ => self.compile_stmt(s),
        }
    }
}

fn compile_x64_fn(
    asm: &mut X64Asm,
    name: &str,
    params: &[String],
    stmts: &[Stmt],
    is_entry: bool,
) -> Result<(), String> {
    asm.fn_offsets.insert(name.to_string(), asm.code.len());
    asm.code.push(0x55); // push rbp
    asm.code.extend_from_slice(&[0x48, 0x89, 0xE5]); // mov rbp, rsp
    asm.code.extend_from_slice(&[0x48, 0x81, 0xEC]); // sub rsp, imm32 (patched later)
    let frame_patch = asm.code.len();
    asm.emit_i32(256);

    let mut vars = HashMap::new();
    for (i, p) in params.iter().enumerate() {
        vars.insert(p.clone(), 16 + (i as i32) * 8);
    }
    let mut ctx = X64FnCtx {
        asm,
        vars,
        next_local_slot: 0,
        loop_stack: Vec::new(),
        epilogue_jumps: Vec::new(),
    };
    for s in stmts {
        ctx.compile_stmt(s)?;
    }
    ctx.asm.code.extend_from_slice(&[0x31, 0xC0]); // default return 0
    let epi = ctx.asm.code.len();
    let frame_bytes = (((ctx.next_local_slot + 1) * 8 + 15) / 16) * 16;
    ctx.asm.code[frame_patch..frame_patch + 4].copy_from_slice(&frame_bytes.to_le_bytes());
    let jmps = std::mem::take(&mut ctx.epilogue_jumps);
    for j in jmps {
        ctx.asm.patch_rel32(j, epi);
    }

    if is_entry {
        ctx.asm.code.extend_from_slice(&[0x89, 0xC1]); // mov ecx, eax (exit code)
        ctx.asm.emit_call_rel32("__rt_exit");
    } else {
        ctx.asm.code.extend_from_slice(&[0x48, 0x89, 0xEC]); // mov rsp, rbp
        ctx.asm.code.push(0x5D); // pop rbp
        ctx.asm.code.push(0xC3); // ret
    }
    Ok(())
}

fn wrap_pe32plus(asm: &mut X64Asm) -> Result<Vec<u8>, String> {
    // Build .idata at VA 0x2000 containing IDT, ILT, IAT (3 imports), hints, and KERNEL32.dll
    let mut idata = vec![0u8; 0x68];
    let emit_hint = |buf: &mut Vec<u8>, name: &str| -> u32 {
        let rva = 0x2000 + buf.len() as u32;
        buf.extend_from_slice(&[0, 0]);
        buf.extend_from_slice(name.as_bytes());
        buf.push(0);
        if (buf.len() & 1) != 0 {
            buf.push(0);
        }
        rva
    };
    let h_exit = emit_hint(&mut idata, "ExitProcess");
    let h_stdout = emit_hint(&mut idata, "GetStdHandle");
    let h_write = emit_hint(&mut idata, "WriteFile");
    let dll_rva = 0x2000 + idata.len() as u32;
    idata.extend_from_slice(b"KERNEL32.dll\0");

    // IDT[0] (20 bytes)
    idata[0..4].copy_from_slice(&(0x2028u32).to_le_bytes()); // OriginalFirstThunk (ILT)
    idata[12..16].copy_from_slice(&dll_rva.to_le_bytes()); // NameRVA
    idata[16..20].copy_from_slice(&(0x2048u32).to_le_bytes()); // FirstThunk (IAT)

    // ILT[0..3] and IAT[0..3]
    for (i, h) in [h_exit, h_stdout, h_write].iter().enumerate() {
        let q = u64::from(*h).to_le_bytes();
        idata[0x28 + i * 8..0x30 + i * 8].copy_from_slice(&q);
        idata[0x48 + i * 8..0x50 + i * 8].copy_from_slice(&q);
    }

    // Align .text and calculate section VirtualAddresses
    let text_raw_size = asm.code.len().div_ceil(0x200) * 0x200;
    let text_vsize = asm.code.len().max(1);
    let text_pages = text_vsize.div_ceil(0x1000) * 0x1000;
    let idata_rva = 0x1000 + text_pages as u32;
    if idata_rva != 0x2000 {
        // Patch all idata RVAs if .text exceeds 4KB
        let delta = idata_rva - 0x2000;
        let bump = |buf: &mut Vec<u8>, off: usize| {
            let v = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap()) + delta;
            buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
        };
        bump(&mut idata, 0);
        bump(&mut idata, 12);
        bump(&mut idata, 16);
        for i in 0..3 {
            let v = u64::from_le_bytes(idata[0x28 + i * 8..0x30 + i * 8].try_into().unwrap())
                + u64::from(delta);
            idata[0x28 + i * 8..0x30 + i * 8].copy_from_slice(&v.to_le_bytes());
            idata[0x48 + i * 8..0x50 + i * 8].copy_from_slice(&v.to_le_bytes());
        }
    }

    // Resolve RIP-relative IAT calls in .text
    let iat_fixups = std::mem::take(&mut asm.iat_fixups);
    for (patch_off, idx) in iat_fixups {
        let next_rip_rva = 0x1000 + (patch_off as i64) + 4;
        let target_iat_rva = i64::from(idata_rva) + 0x48 + (idx as i64) * 8;
        let disp = (target_iat_rva - next_rip_rva) as i32;
        asm.code[patch_off..patch_off + 4].copy_from_slice(&disp.to_le_bytes());
    }

    let idata_raw_size = idata.len().div_ceil(0x200) * 0x200;
    let idata_vsize = idata.len();
    let image_size = idata_rva + (idata_vsize as u32).div_ceil(0x1000) * 0x1000;

    let text_file_ptr = 0x200u32;
    let idata_file_ptr = text_file_ptr + text_raw_size as u32;
    let mut out = vec![0u8; (idata_file_ptr + idata_raw_size as u32) as usize];

    // DOS Header
    out[0] = b'M';
    out[1] = b'Z';
    out[0x3C..0x40].copy_from_slice(&0x80u32.to_le_bytes()); // e_lfanew = 0x80

    // NT Signature + COFF Header at 0x80
    let pe = 0x80usize;
    out[pe..pe + 4].copy_from_slice(b"PE\0\0");
    out[pe + 4..pe + 6].copy_from_slice(&0x8664u16.to_le_bytes()); // AMD64
    out[pe + 6..pe + 8].copy_from_slice(&2u16.to_le_bytes()); // 2 sections
    out[pe + 20..pe + 22].copy_from_slice(&0xF0u16.to_le_bytes()); // OptionalHeader size = 240
    out[pe + 22..pe + 24].copy_from_slice(&0x0022u16.to_le_bytes()); // EXECUTABLE | LARGE_ADDRESS_AWARE

    // Optional Header PE32+ at pe + 24 (0x98..0x188)
    let opt = pe + 24;
    out[opt..opt + 2].copy_from_slice(&0x020Bu16.to_le_bytes()); // Magic = PE32+
    out[opt + 4..opt + 8].copy_from_slice(&(text_raw_size as u32).to_le_bytes());
    out[opt + 8..opt + 12].copy_from_slice(&(idata_raw_size as u32).to_le_bytes());
    out[opt + 16..opt + 20].copy_from_slice(&0x1000u32.to_le_bytes()); // AddressOfEntryPoint
    out[opt + 20..opt + 24].copy_from_slice(&0x1000u32.to_le_bytes()); // BaseOfCode
    out[opt + 24..opt + 32].copy_from_slice(&0x0000_0001_4000_0000u64.to_le_bytes()); // ImageBase
    out[opt + 32..opt + 36].copy_from_slice(&0x1000u32.to_le_bytes()); // SectionAlignment
    out[opt + 36..opt + 40].copy_from_slice(&0x0200u32.to_le_bytes()); // FileAlignment
    out[opt + 40..opt + 42].copy_from_slice(&6u16.to_le_bytes()); // MajorOSVersion
    out[opt + 48..opt + 50].copy_from_slice(&6u16.to_le_bytes()); // MajorSubsystemVersion
    out[opt + 56..opt + 60].copy_from_slice(&image_size.to_le_bytes()); // SizeOfImage
    out[opt + 60..opt + 64].copy_from_slice(&0x0200u32.to_le_bytes()); // SizeOfHeaders
    out[opt + 68..opt + 70].copy_from_slice(&3u16.to_le_bytes()); // CONSOLE
    out[opt + 70..opt + 72].copy_from_slice(&0x0100u16.to_le_bytes()); // NX_COMPAT
    out[opt + 72..opt + 80].copy_from_slice(&0x100000u64.to_le_bytes()); // StackReserve
    out[opt + 80..opt + 88].copy_from_slice(&0x1000u64.to_le_bytes()); // StackCommit
    out[opt + 88..opt + 96].copy_from_slice(&0x100000u64.to_le_bytes()); // HeapReserve
    out[opt + 96..opt + 104].copy_from_slice(&0x1000u64.to_le_bytes()); // HeapCommit
    out[opt + 108..opt + 112].copy_from_slice(&16u32.to_le_bytes()); // NumberOfRvaAndSizes

    // DataDirectory[1] (Import) & DataDirectory[12] (IAT)
    let dd = opt + 112;
    out[dd + 8..dd + 12].copy_from_slice(&idata_rva.to_le_bytes());
    out[dd + 12..dd + 16].copy_from_slice(&40u32.to_le_bytes());
    out[dd + 12 * 8..dd + 12 * 8 + 4].copy_from_slice(&(idata_rva + 0x48).to_le_bytes());
    out[dd + 12 * 8 + 4..dd + 12 * 8 + 8].copy_from_slice(&32u32.to_le_bytes());

    // Section Table right after OptionalHeader (pe + 24 + 240 = 0x188)
    let sh = opt + 0xF0;
    let write_sec = |buf: &mut [u8],
                     off: usize,
                     name: &[u8; 8],
                     vsize: u32,
                     vaddr: u32,
                     rsize: u32,
                     rptr: u32,
                     chars: u32| {
        buf[off..off + 8].copy_from_slice(name);
        buf[off + 8..off + 12].copy_from_slice(&vsize.to_le_bytes());
        buf[off + 12..off + 16].copy_from_slice(&vaddr.to_le_bytes());
        buf[off + 16..off + 20].copy_from_slice(&rsize.to_le_bytes());
        buf[off + 20..off + 24].copy_from_slice(&rptr.to_le_bytes());
        buf[off + 36..off + 40].copy_from_slice(&chars.to_le_bytes());
    };
    write_sec(
        &mut out,
        sh,
        b".text\0\0\0",
        text_vsize as u32,
        0x1000,
        text_raw_size as u32,
        text_file_ptr,
        0x6000_0020,
    );
    write_sec(
        &mut out,
        sh + 40,
        b".idata\0\0",
        idata_vsize as u32,
        idata_rva,
        idata_raw_size as u32,
        idata_file_ptr,
        0xC000_0040,
    );

    out[text_file_ptr as usize..text_file_ptr as usize + asm.code.len()].copy_from_slice(&asm.code);
    out[idata_file_ptr as usize..idata_file_ptr as usize + idata.len()].copy_from_slice(&idata);
    Ok(out)
}

fn wrap_elf64(asm: &X64Asm) -> Vec<u8> {
    let hdr_size = 64usize + 56usize; // ELF64 header + 1 PT_LOAD phdr
    let total = hdr_size + asm.code.len();
    let vaddr = 0x400000u64;
    let entry = vaddr + hdr_size as u64;
    let mut out = vec![0u8; total];

    // ELF64 Header
    out[0..4].copy_from_slice(b"\x7FELF");
    out[4] = 2; // 64-bit
    out[5] = 1; // little-endian
    out[6] = 1; // EV_CURRENT
    out[16..18].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC
    out[18..20].copy_from_slice(&62u16.to_le_bytes()); // EM_X86_64
    out[20..24].copy_from_slice(&1u32.to_le_bytes()); // e_version
    out[24..32].copy_from_slice(&entry.to_le_bytes()); // e_entry
    out[32..40].copy_from_slice(&64u64.to_le_bytes()); // e_phoff
    out[52..54].copy_from_slice(&64u16.to_le_bytes()); // e_ehsize
    out[54..56].copy_from_slice(&56u16.to_le_bytes()); // e_phentsize
    out[56..58].copy_from_slice(&1u16.to_le_bytes()); // e_phnum

    // PT_LOAD Program Header at 64
    let ph = 64usize;
    out[ph..ph + 4].copy_from_slice(&1u32.to_le_bytes()); // PT_LOAD
    out[ph + 4..ph + 8].copy_from_slice(&5u32.to_le_bytes()); // PF_R | PF_X
    out[ph + 8..ph + 16].copy_from_slice(&0u64.to_le_bytes()); // p_offset
    out[ph + 16..ph + 24].copy_from_slice(&vaddr.to_le_bytes()); // p_vaddr
    out[ph + 24..ph + 32].copy_from_slice(&vaddr.to_le_bytes()); // p_paddr
    out[ph + 32..ph + 40].copy_from_slice(&(total as u64).to_le_bytes()); // p_filesz
    out[ph + 40..ph + 48].copy_from_slice(&(total as u64).to_le_bytes()); // p_memsz
    out[ph + 48..ph + 56].copy_from_slice(&0x1000u64.to_le_bytes()); // p_align

    out[hdr_size..].copy_from_slice(&asm.code);
    out
}

pub(crate) fn compile_source_to_direct_binary(
    src: &str,
    fmt: DirectNativeFormat,
) -> Result<Vec<u8>, String> {
    let tokens = lex_with_lines(src)?;
    let ast = Parser::new_with_lines(tokens).program()?;
    semantic_check_ast(&ast)?;

    let mut defs: Vec<(String, Vec<String>, Vec<Stmt>)> = Vec::new();
    let mut top_stmts: Vec<Stmt> = Vec::new();
    for s in ast {
        match s {
            Stmt::Fn(n, ps, _, b)
            | Stmt::AsyncFn(n, ps, _, b)
            | Stmt::DecoratedFn(_, n, ps, _, b) => {
                defs.push((n, ps.into_iter().map(|(p, _)| p).collect(), b));
            }
            other => top_stmts.push(other),
        }
    }

    let mut asm = X64Asm::new();
    // Emit entry point at offset 0 of .text so AddressOfEntryPoint / e_entry points directly here!
    compile_x64_fn(&mut asm, "__entry", &[], &top_stmts, true)?;
    for (n, ps, b) in defs {
        compile_x64_fn(&mut asm, &n, &ps, &b, false)?;
    }
    asm.emit_runtime_stubs(fmt);

    let call_fixups = std::mem::take(&mut asm.call_fixups);
    for (patch_off, target_name) in call_fixups {
        let target_off = *asm
            .fn_offsets
            .get(&target_name)
            .ok_or_else(|| format!("direct-aot: undefined function '{target_name}'"))?;
        asm.patch_rel32(patch_off, target_off);
    }

    match fmt {
        DirectNativeFormat::Pe64 => wrap_pe32plus(&mut asm),
        DirectNativeFormat::Elf64 => Ok(wrap_elf64(&asm)),
    }
}

pub(crate) fn compile_file_to_direct_native(
    input: &Path,
    output: Option<&Path>,
    fmt_override: Option<DirectNativeFormat>,
) -> Result<PathBuf, String> {
    let src = load_source(input.to_string_lossy().as_ref())?;
    let out_path = match output {
        Some(p) => p.to_path_buf(),
        None => {
            if matches!(fmt_override, Some(DirectNativeFormat::Elf64))
                || !cfg!(target_os = "windows")
            {
                let c = input.with_extension("");
                if c == input {
                    input.with_extension("out")
                } else {
                    c
                }
            } else {
                input.with_extension("exe")
            }
        }
    };
    let fmt = fmt_override.unwrap_or_else(|| DirectNativeFormat::for_path(&out_path));
    let bin = compile_source_to_direct_binary(&src, fmt)?;
    if let Some(parent) = out_path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::write(&out_path, &bin).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(&out_path) {
            let mut perms = meta.permissions();
            perms.set_mode(0o755);
            let _ = fs::set_permissions(&out_path, perms);
        }
    }
    Ok(out_path)
}
