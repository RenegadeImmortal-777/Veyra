// Native AOT compiler backend: compiles Kovrek AST to standalone optimized
// Rust machine code via `rustc`. Works offline with zero extra dependencies
// and compiles cleanly under `--no-default-features`.

use std::{
    collections::{BTreeSet, HashMap},
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

use crate::ast::{CallArg, Expr, Pattern, SeqElem, Stmt};
use crate::lexer::lex_with_lines;
use crate::parser::Parser;
use crate::pipeline::{load_source, semantic_check_ast};

static AOT_TEMP_IDS: AtomicU64 = AtomicU64::new(1);

struct TempFileGuard(PathBuf);

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum VarKind {
    Plain,
    Cell,
    Captured,
    Global,
}

#[derive(Clone)]
struct ScopeInfo {
    is_top: bool,
    vars: HashMap<String, VarKind>,
    strict_vars: BTreeSet<String>,
    captured_cells: BTreeSet<String>,
    known_globals: BTreeSet<String>,
}

impl ScopeInfo {
    fn resolve(&self, name: &str) -> VarKind {
        self.vars.get(name).copied().unwrap_or(VarKind::Global)
    }
}

fn mangle_ident(name: &str) -> String {
    let mut out = String::from("vy_v_");
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch);
        } else {
            out.push_str(&format!("_x{:x}_", ch as u32));
        }
    }
    out
}

fn escape_rust_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn stmts_have_yield(stmts: &[Stmt]) -> bool {
    for stmt in stmts {
        match stmt {
            Stmt::Yield(_) => return true,
            Stmt::If(_, t, e) => {
                if stmts_have_yield(t) || stmts_have_yield(e) {
                    return true;
                }
            }
            Stmt::While(_, b)
            | Stmt::ForRange(_, _, _, b)
            | Stmt::ForEach(_, _, b)
            | Stmt::With(_, _, b) => {
                if stmts_have_yield(b) {
                    return true;
                }
            }
            Stmt::Try(b, _, c, f) => {
                if stmts_have_yield(b) || stmts_have_yield(c) || stmts_have_yield(f) {
                    return true;
                }
            }
            Stmt::Match(_, arms) => {
                for (_, _, b) in arms {
                    if stmts_have_yield(b) {
                        return true;
                    }
                }
            }
            _ => {}
        }
    }
    false
}

fn validate_call_arg(arg: &CallArg) -> Result<(), String> {
    match arg {
        CallArg::Pos(e) | CallArg::Star(e) | CallArg::Kw(_, e) | CallArg::KwStar(e) => {
            validate_aot_expr(e)
        }
    }
}

fn validate_aot_pattern(pat: &Pattern) -> Result<(), String> {
    match pat {
        Pattern::Wildcard | Pattern::Capture(_) => Ok(()),
        Pattern::Literal(e) => validate_aot_expr(e),
        Pattern::Or(alts) => {
            for a in alts {
                validate_aot_pattern(a)?;
            }
            Ok(())
        }
        Pattern::As(inner, _) => validate_aot_pattern(inner),
        Pattern::Seq(elems) => {
            for el in elems {
                if let SeqElem::Pat(p) = el {
                    validate_aot_pattern(p)?;
                }
            }
            Ok(())
        }
        Pattern::Map { entries, .. } => {
            for (k, v) in entries {
                validate_aot_expr(k)?;
                validate_aot_pattern(v)?;
            }
            Ok(())
        }
        Pattern::Class { cls, pos, kw } => {
            validate_aot_expr(cls)?;
            for p in pos {
                validate_aot_pattern(p)?;
            }
            for (_, p) in kw {
                validate_aot_pattern(p)?;
            }
            Ok(())
        }
    }
}

fn collect_pattern_bound_names(pat: &Pattern, out: &mut BTreeSet<String>) {
    match pat {
        Pattern::Wildcard | Pattern::Literal(_) => {}
        Pattern::Capture(name) => {
            out.insert(name.clone());
        }
        Pattern::Or(alts) => {
            for a in alts {
                collect_pattern_bound_names(a, out);
            }
        }
        Pattern::As(inner, name) => {
            collect_pattern_bound_names(inner, out);
            out.insert(name.clone());
        }
        Pattern::Seq(elems) => {
            for el in elems {
                match el {
                    SeqElem::Pat(p) => collect_pattern_bound_names(p, out),
                    SeqElem::Star(Some(name)) => {
                        out.insert(name.clone());
                    }
                    SeqElem::Star(None) => {}
                }
            }
        }
        Pattern::Map { entries, rest } => {
            for (_, p) in entries {
                collect_pattern_bound_names(p, out);
            }
            if let Some(r) = rest {
                out.insert(r.clone());
            }
        }
        Pattern::Class { pos, kw, .. } => {
            for p in pos {
                collect_pattern_bound_names(p, out);
            }
            for (_, p) in kw {
                collect_pattern_bound_names(p, out);
            }
        }
    }
}

fn collect_all_ids_pattern(pat: &Pattern, out: &mut BTreeSet<String>) {
    match pat {
        Pattern::Wildcard => {}
        Pattern::Capture(name) => {
            out.insert(name.clone());
        }
        Pattern::Literal(e) => collect_all_ids_expr(e, out),
        Pattern::Or(alts) => {
            for a in alts {
                collect_all_ids_pattern(a, out);
            }
        }
        Pattern::As(inner, name) => {
            collect_all_ids_pattern(inner, out);
            out.insert(name.clone());
        }
        Pattern::Seq(elems) => {
            for el in elems {
                match el {
                    SeqElem::Pat(p) => collect_all_ids_pattern(p, out),
                    SeqElem::Star(Some(name)) => {
                        out.insert(name.clone());
                    }
                    SeqElem::Star(None) => {}
                }
            }
        }
        Pattern::Map { entries, rest } => {
            for (k, v) in entries {
                collect_all_ids_expr(k, out);
                collect_all_ids_pattern(v, out);
            }
            if let Some(r) = rest {
                out.insert(r.clone());
            }
        }
        Pattern::Class { cls, pos, kw } => {
            collect_all_ids_expr(cls, out);
            for p in pos {
                collect_all_ids_pattern(p, out);
            }
            for (_, p) in kw {
                collect_all_ids_pattern(p, out);
            }
        }
    }
}

fn collect_nested_refs_pattern(pat: &Pattern, out: &mut BTreeSet<String>) {
    match pat {
        Pattern::Wildcard | Pattern::Capture(_) => {}
        Pattern::Literal(e) => collect_nested_refs_expr(e, out),
        Pattern::Or(alts) => {
            for a in alts {
                collect_nested_refs_pattern(a, out);
            }
        }
        Pattern::As(inner, _) => collect_nested_refs_pattern(inner, out),
        Pattern::Seq(elems) => {
            for el in elems {
                if let SeqElem::Pat(p) = el {
                    collect_nested_refs_pattern(p, out);
                }
            }
        }
        Pattern::Map { entries, .. } => {
            for (k, v) in entries {
                collect_nested_refs_expr(k, out);
                collect_nested_refs_pattern(v, out);
            }
        }
        Pattern::Class { cls, pos, kw } => {
            collect_nested_refs_expr(cls, out);
            for p in pos {
                collect_nested_refs_pattern(p, out);
            }
            for (_, p) in kw {
                collect_nested_refs_pattern(p, out);
            }
        }
    }
}

fn validate_aot_support(stmts: &[Stmt]) -> Result<(), String> {
    for stmt in stmts {
        match stmt {
            Stmt::If(c, t, e) => {
                validate_aot_expr(c)?;
                validate_aot_support(t)?;
                validate_aot_support(e)?;
            }
            Stmt::While(c, b) => {
                validate_aot_expr(c)?;
                validate_aot_support(b)?;
            }
            Stmt::ForRange(_, a, b, body) => {
                validate_aot_expr(a)?;
                validate_aot_expr(b)?;
                validate_aot_support(body)?;
            }
            Stmt::ForEach(_, e, body) | Stmt::With(e, _, body) => {
                validate_aot_expr(e)?;
                validate_aot_support(body)?;
            }
            Stmt::Try(b, _, c, f) => {
                validate_aot_support(b)?;
                validate_aot_support(c)?;
                validate_aot_support(f)?;
            }
            Stmt::Match(subj, arms) => {
                validate_aot_expr(subj)?;
                for (p, g, b) in arms {
                    validate_aot_pattern(p)?;
                    if let Some(ge) = g {
                        validate_aot_expr(ge)?;
                    }
                    validate_aot_support(b)?;
                }
            }
            Stmt::Fn(_, _, _, body) | Stmt::AsyncFn(_, _, _, body) => {
                validate_aot_support(body)?;
            }
            Stmt::DecoratedFn(decs, _, _, _, body) => {
                for d in decs {
                    if !crate::pipeline::is_static_effect_decorator(d) {
                        validate_aot_expr(d)?;
                    }
                }
                validate_aot_support(body)?;
            }
            Stmt::Class(_, bases, meta, decs, body) => {
                for b in bases {
                    validate_aot_expr(b)?;
                }
                if let Some(m) = meta {
                    validate_aot_expr(m)?;
                }
                for d in decs {
                    validate_aot_expr(d)?;
                }
                validate_aot_support(body)?;
            }
            Stmt::Let(_, _, e, _) | Stmt::Assign(_, e) | Stmt::Expr(e) | Stmt::Throw(e) => {
                validate_aot_expr(e)?;
            }
            Stmt::SetMember(a, _, b) => {
                validate_aot_expr(a)?;
                validate_aot_expr(b)?;
            }
            Stmt::SetIndex(a, b, c) => {
                validate_aot_expr(a)?;
                validate_aot_expr(b)?;
                validate_aot_expr(c)?;
            }
            Stmt::Return(Some(e)) | Stmt::Yield(e) => validate_aot_expr(e)?,
            Stmt::Return(None) | Stmt::Break | Stmt::Continue | Stmt::SourceLine(_) => {}
        }
    }
    Ok(())
}

fn validate_aot_expr(expr: &Expr) -> Result<(), String> {
    match expr {
        Expr::Await(a) => validate_aot_expr(a),
        Expr::List(xs) => {
            for x in xs {
                validate_aot_expr(x)?;
            }
            Ok(())
        }
        Expr::Map(pairs) => {
            for (k, v) in pairs {
                validate_aot_expr(k)?;
                validate_aot_expr(v)?;
            }
            Ok(())
        }
        Expr::Index(a, b) | Expr::Bin(a, _, b) => {
            validate_aot_expr(a)?;
            validate_aot_expr(b)
        }
        Expr::ChainedCmp(first, rest) => {
            validate_aot_expr(first)?;
            for (_, rhs) in rest {
                validate_aot_expr(rhs)?;
            }
            Ok(())
        }
        Expr::Member(a, _) | Expr::Unary(_, a) => validate_aot_expr(a),
        Expr::MethodCall(a, _, args) => {
            validate_aot_expr(a)?;
            for arg in args {
                validate_aot_expr(arg)?;
            }
            Ok(())
        }
        Expr::Call(_, args) => {
            for arg in args {
                validate_aot_expr(arg)?;
            }
            Ok(())
        }
        Expr::CallExpr(callee, args) => {
            validate_aot_expr(callee)?;
            for arg in args {
                validate_aot_expr(arg)?;
            }
            Ok(())
        }
        Expr::CallExt { callee, args, .. } => {
            validate_aot_expr(callee)?;
            for arg in args {
                validate_call_arg(arg)?;
            }
            Ok(())
        }
        Expr::Ternary(a, b, c) => {
            validate_aot_expr(a)?;
            validate_aot_expr(b)?;
            validate_aot_expr(c)
        }
        Expr::Lambda(_, _, body) => validate_aot_support(body),
        Expr::ListComp(e, _, iter, cond) => {
            validate_aot_expr(e)?;
            validate_aot_expr(iter)?;
            if let Some(c) = cond {
                validate_aot_expr(c)?;
            }
            Ok(())
        }
        Expr::Num(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Null | Expr::Var(_) => Ok(()),
    }
}

fn collect_scope_decls(
    stmts: &[Stmt],
    is_top: bool,
    decls: &mut BTreeSet<String>,
    explicit_decls: &mut BTreeSet<String>,
    strict: &mut BTreeSet<String>,
    enclosing: &BTreeSet<String>,
) {
    for stmt in stmts {
        match stmt {
            Stmt::Let(name, ty, _, is_const) => {
                decls.insert(name.clone());
                explicit_decls.insert(name.clone());
                if *is_const || ty.is_some() {
                    strict.insert(name.clone());
                }
            }
            Stmt::Assign(name, _) => {
                if !enclosing.contains(name) {
                    decls.insert(name.clone());
                }
            }
            Stmt::ForRange(name, _, _, body)
            | Stmt::ForEach(name, _, body)
            | Stmt::With(_, name, body) => {
                decls.insert(name.clone());
                explicit_decls.insert(name.clone());
                collect_scope_decls(body, is_top, decls, explicit_decls, strict, enclosing);
            }
            Stmt::Try(b, err_var, c, f) => {
                collect_scope_decls(b, is_top, decls, explicit_decls, strict, enclosing);
                decls.insert(err_var.clone());
                explicit_decls.insert(err_var.clone());
                collect_scope_decls(c, is_top, decls, explicit_decls, strict, enclosing);
                collect_scope_decls(f, is_top, decls, explicit_decls, strict, enclosing);
            }
            Stmt::If(_, t, e) => {
                collect_scope_decls(t, is_top, decls, explicit_decls, strict, enclosing);
                collect_scope_decls(e, is_top, decls, explicit_decls, strict, enclosing);
            }
            Stmt::While(_, b) => {
                collect_scope_decls(b, is_top, decls, explicit_decls, strict, enclosing)
            }
            Stmt::Match(_, arms) => {
                for (p, _, b) in arms {
                    if !is_top {
                        collect_pattern_bound_names(p, decls);
                    }
                    collect_scope_decls(b, is_top, decls, explicit_decls, strict, enclosing);
                }
            }
            Stmt::Fn(name, _, _, _)
            | Stmt::AsyncFn(name, _, _, _)
            | Stmt::DecoratedFn(_, name, _, _, _)
            | Stmt::Class(name, _, _, _, _) => {
                if !is_top {
                    decls.insert(name.clone());
                    explicit_decls.insert(name.clone());
                }
            }
            _ => {}
        }
    }
}

fn collect_all_ids_call_arg(arg: &CallArg, out: &mut BTreeSet<String>) {
    match arg {
        CallArg::Pos(e) | CallArg::Star(e) | CallArg::Kw(_, e) | CallArg::KwStar(e) => {
            collect_all_ids_expr(e, out)
        }
    }
}

fn collect_nested_refs_call_arg(arg: &CallArg, out: &mut BTreeSet<String>) {
    match arg {
        CallArg::Pos(e) | CallArg::Star(e) | CallArg::Kw(_, e) | CallArg::KwStar(e) => {
            collect_nested_refs_expr(e, out)
        }
    }
}

fn collect_all_ids_expr(expr: &Expr, out: &mut BTreeSet<String>) {
    match expr {
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
        Expr::CallExt { callee, args, .. } => {
            collect_all_ids_expr(callee, out);
            for a in args {
                collect_all_ids_call_arg(a, out);
            }
        }
        Expr::MethodCall(recv, _, args) => {
            collect_all_ids_expr(recv, out);
            for a in args {
                collect_all_ids_expr(a, out);
            }
        }
        Expr::List(xs) => {
            for x in xs {
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
        Expr::ChainedCmp(first, rest) => {
            collect_all_ids_expr(first, out);
            for (_, rhs) in rest {
                collect_all_ids_expr(rhs, out);
            }
        }
        Expr::Member(a, _) | Expr::Await(a) | Expr::Unary(_, a) => collect_all_ids_expr(a, out),
        Expr::Ternary(a, b, c) => {
            collect_all_ids_expr(a, out);
            collect_all_ids_expr(b, out);
            collect_all_ids_expr(c, out);
        }
        Expr::Lambda(_, _, body) => collect_all_ids_stmts(body, out),
        Expr::ListComp(e, _, iter, cond) => {
            collect_all_ids_expr(e, out);
            collect_all_ids_expr(iter, out);
            if let Some(c) = cond {
                collect_all_ids_expr(c, out);
            }
        }
        Expr::Num(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Null => {}
    }
}

fn collect_all_ids_stmts(stmts: &[Stmt], out: &mut BTreeSet<String>) {
    for s in stmts {
        match s {
            Stmt::Let(n, _, e, _) | Stmt::Assign(n, e) => {
                out.insert(n.clone());
                collect_all_ids_expr(e, out);
            }
            Stmt::Expr(e) | Stmt::Throw(e) => collect_all_ids_expr(e, out),
            Stmt::Return(Some(e)) | Stmt::Yield(e) => collect_all_ids_expr(e, out),
            Stmt::If(c, t, e) => {
                collect_all_ids_expr(c, out);
                collect_all_ids_stmts(t, out);
                collect_all_ids_stmts(e, out);
            }
            Stmt::While(c, b) => {
                collect_all_ids_expr(c, out);
                collect_all_ids_stmts(b, out);
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
            Stmt::Try(b, err_var, c, f) => {
                out.insert(err_var.clone());
                collect_all_ids_stmts(b, out);
                collect_all_ids_stmts(c, out);
                collect_all_ids_stmts(f, out);
            }
            Stmt::Fn(n, _, _, body) | Stmt::AsyncFn(n, _, _, body) => {
                out.insert(n.clone());
                collect_all_ids_stmts(body, out);
            }
            Stmt::DecoratedFn(decs, n, _, _, body) => {
                out.insert(n.clone());
                for d in decs {
                    if !crate::pipeline::is_static_effect_decorator(d) {
                        collect_all_ids_expr(d, out);
                    }
                }
                collect_all_ids_stmts(body, out);
            }
            Stmt::Class(n, bases, meta, decs, body) => {
                out.insert(n.clone());
                for b in bases {
                    collect_all_ids_expr(b, out);
                }
                if let Some(m) = meta {
                    collect_all_ids_expr(m, out);
                }
                for d in decs {
                    collect_all_ids_expr(d, out);
                }
                collect_all_ids_stmts(body, out);
            }
            Stmt::SetMember(a, _, b) => {
                collect_all_ids_expr(a, out);
                collect_all_ids_expr(b, out);
            }
            Stmt::SetIndex(a, b, c) => {
                collect_all_ids_expr(a, out);
                collect_all_ids_expr(b, out);
                collect_all_ids_expr(c, out);
            }
            Stmt::Match(e, arms) => {
                collect_all_ids_expr(e, out);
                for (p, g, b) in arms {
                    collect_all_ids_pattern(p, out);
                    if let Some(ge) = g {
                        collect_all_ids_expr(ge, out);
                    }
                    collect_all_ids_stmts(b, out);
                }
            }
            Stmt::Return(None) | Stmt::Break | Stmt::Continue | Stmt::SourceLine(_) => {}
        }
    }
}

fn collect_nested_refs_expr(expr: &Expr, out: &mut BTreeSet<String>) {
    match expr {
        Expr::Lambda(_, _, body) => collect_all_ids_stmts(body, out),
        Expr::ListComp(e, _, iter, cond) => {
            collect_all_ids_expr(e, out);
            collect_nested_refs_expr(iter, out);
            if let Some(c) = cond {
                collect_all_ids_expr(c, out);
            }
        }
        Expr::List(xs) => {
            for x in xs {
                collect_nested_refs_expr(x, out);
            }
        }
        Expr::Map(pairs) => {
            for (k, v) in pairs {
                collect_nested_refs_expr(k, out);
                collect_nested_refs_expr(v, out);
            }
        }
        Expr::Index(a, b) | Expr::Bin(a, _, b) => {
            collect_nested_refs_expr(a, out);
            collect_nested_refs_expr(b, out);
        }
        Expr::ChainedCmp(first, rest) => {
            collect_nested_refs_expr(first, out);
            for (_, rhs) in rest {
                collect_nested_refs_expr(rhs, out);
            }
        }
        Expr::Member(a, _) | Expr::Await(a) | Expr::Unary(_, a) => collect_nested_refs_expr(a, out),
        Expr::MethodCall(recv, _, args) => {
            collect_nested_refs_expr(recv, out);
            for a in args {
                collect_nested_refs_expr(a, out);
            }
        }
        Expr::Call(_, args) => {
            for a in args {
                collect_nested_refs_expr(a, out);
            }
        }
        Expr::CallExpr(callee, args) => {
            collect_nested_refs_expr(callee, out);
            for a in args {
                collect_nested_refs_expr(a, out);
            }
        }
        Expr::CallExt { callee, args, .. } => {
            collect_nested_refs_expr(callee, out);
            for a in args {
                collect_nested_refs_call_arg(a, out);
            }
        }
        Expr::Ternary(a, b, c) => {
            collect_nested_refs_expr(a, out);
            collect_nested_refs_expr(b, out);
            collect_nested_refs_expr(c, out);
        }
        Expr::Num(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Null | Expr::Var(_) => {}
    }
}

fn collect_nested_refs_stmts(stmts: &[Stmt], out: &mut BTreeSet<String>) {
    for s in stmts {
        match s {
            Stmt::Fn(name, _, _, body) | Stmt::AsyncFn(name, _, _, body) => {
                out.insert(name.clone());
                collect_all_ids_stmts(body, out);
            }
            Stmt::DecoratedFn(decs, name, _, _, body) => {
                out.insert(name.clone());
                for d in decs {
                    if !crate::pipeline::is_static_effect_decorator(d) {
                        collect_all_ids_expr(d, out);
                    }
                }
                collect_all_ids_stmts(body, out);
            }
            Stmt::Class(name, bases, meta, decs, body) => {
                out.insert(name.clone());
                for b in bases {
                    collect_all_ids_expr(b, out);
                }
                if let Some(m) = meta {
                    collect_all_ids_expr(m, out);
                }
                for d in decs {
                    collect_all_ids_expr(d, out);
                }
                collect_all_ids_stmts(body, out);
            }
            Stmt::Let(_, _, e, _) | Stmt::Assign(_, e) | Stmt::Expr(e) | Stmt::Throw(e) => {
                collect_nested_refs_expr(e, out);
            }
            Stmt::Return(Some(e)) | Stmt::Yield(e) => collect_nested_refs_expr(e, out),
            Stmt::If(c, t, e) => {
                collect_nested_refs_expr(c, out);
                collect_nested_refs_stmts(t, out);
                collect_nested_refs_stmts(e, out);
            }
            Stmt::While(c, b) => {
                collect_nested_refs_expr(c, out);
                collect_nested_refs_stmts(b, out);
            }
            Stmt::ForRange(_, a, b, body) => {
                collect_nested_refs_expr(a, out);
                collect_nested_refs_expr(b, out);
                collect_nested_refs_stmts(body, out);
            }
            Stmt::ForEach(_, e, body) | Stmt::With(e, _, body) => {
                collect_nested_refs_expr(e, out);
                collect_nested_refs_stmts(body, out);
            }
            Stmt::Try(b, _, c, f) => {
                collect_nested_refs_stmts(b, out);
                collect_nested_refs_stmts(c, out);
                collect_nested_refs_stmts(f, out);
            }
            Stmt::SetMember(a, _, b) => {
                collect_nested_refs_expr(a, out);
                collect_nested_refs_expr(b, out);
            }
            Stmt::SetIndex(a, b, c) => {
                collect_nested_refs_expr(a, out);
                collect_nested_refs_expr(b, out);
                collect_nested_refs_expr(c, out);
            }
            Stmt::Match(e, arms) => {
                collect_nested_refs_expr(e, out);
                for (p, g, b) in arms {
                    collect_nested_refs_pattern(p, out);
                    if let Some(ge) = g {
                        collect_nested_refs_expr(ge, out);
                    }
                    collect_nested_refs_stmts(b, out);
                }
            }
            Stmt::Return(None) | Stmt::Break | Stmt::Continue | Stmt::SourceLine(_) => {}
        }
    }
}

struct AotEmitter {
    indent: usize,
    out: String,
    temp_counter: usize,
    in_generator: bool,
}

impl AotEmitter {
    fn new() -> Self {
        Self {
            indent: 0,
            out: String::new(),
            temp_counter: 0,
            in_generator: false,
        }
    }

    fn fresh_tmp(&mut self, prefix: &str) -> String {
        let id = self.temp_counter;
        self.temp_counter += 1;
        format!("vy_t_{prefix}_{id}")
    }

    fn line(&mut self, s: &str) {
        for _ in 0..self.indent {
            self.out.push_str("    ");
        }
        self.out.push_str(s);
        self.out.push('\n');
    }

    fn build_scope(
        &self,
        is_top: bool,
        params: &[(String, Option<String>)],
        stmts: &[Stmt],
        parent_cells: &BTreeSet<String>,
        parent_globals: &BTreeSet<String>,
    ) -> ScopeInfo {
        let mut enclosing = parent_cells.clone();
        for g in parent_globals {
            enclosing.insert(g.clone());
        }
        for (p, _) in params {
            enclosing.insert(p.clone());
        }
        let mut decls = BTreeSet::new();
        let mut explicit_decls = BTreeSet::new();
        let mut strict = BTreeSet::new();
        collect_scope_decls(
            stmts,
            is_top,
            &mut decls,
            &mut explicit_decls,
            &mut strict,
            &enclosing,
        );
        for (p, _) in params {
            explicit_decls.insert(p.clone());
        }
        let mut nested_refs = BTreeSet::new();
        collect_nested_refs_stmts(stmts, &mut nested_refs);

        let mut vars = HashMap::new();
        let mut captured_cells = BTreeSet::new();
        let mut known_globals = BTreeSet::new();

        for g in parent_globals {
            if !explicit_decls.contains(g) {
                known_globals.insert(g.clone());
            }
        }
        for name in parent_cells {
            if !explicit_decls.contains(name) {
                vars.insert(name.clone(), VarKind::Captured);
                captured_cells.insert(name.clone());
            }
        }
        for (p, ty) in params {
            if ty.is_some() {
                strict.insert(p.clone());
            }
            if nested_refs.contains(p) {
                vars.insert(p.clone(), VarKind::Cell);
                captured_cells.insert(p.clone());
            } else {
                vars.insert(p.clone(), VarKind::Plain);
            }
        }
        for d in decls {
            if vars.contains_key(&d) {
                continue;
            }
            if is_top && nested_refs.contains(&d) {
                known_globals.insert(d.clone());
                vars.insert(d, VarKind::Global);
            } else if nested_refs.contains(&d) {
                vars.insert(d.clone(), VarKind::Cell);
                captured_cells.insert(d);
            } else {
                vars.insert(d, VarKind::Plain);
            }
        }

        ScopeInfo {
            is_top,
            vars,
            strict_vars: strict,
            captured_cells,
            known_globals,
        }
    }

    fn emit_var_decls(&mut self, scope: &ScopeInfo, skip_params: &[(String, Option<String>)]) {
        let param_set: BTreeSet<&str> = skip_params.iter().map(|(p, _)| p.as_str()).collect();
        let mut names: Vec<(&String, &VarKind)> = scope.vars.iter().collect();
        names.sort_by(|a, b| a.0.cmp(b.0));
        for (name, kind) in names {
            if param_set.contains(name.as_str()) {
                continue;
            }
            let ident = mangle_ident(name);
            match kind {
                VarKind::Plain => {
                    self.line(&format!("let mut {ident}: Val = Val::Null;"));
                }
                VarKind::Cell => {
                    self.line(&format!(
                        "let {ident}: Rc<RefCell<Val>> = Rc::new(RefCell::new(Val::Null));"
                    ));
                }
                VarKind::Captured | VarKind::Global => {}
            }
            if scope.strict_vars.contains(name) && *kind != VarKind::Captured {
                let cflag = format!("{ident}_const");
                let dflag = format!("{ident}_def");
                let tflag = format!("{ident}_ty");
                self.line(&format!("let mut {dflag}: bool = false;"));
                self.line(&format!("let mut {cflag}: bool = false;"));
                self.line(&format!("let mut {tflag}: Option<&'static str> = None;"));
            }
        }
    }

    fn emit_read_var(&self, scope: &ScopeInfo, name: &str) -> String {
        let ident = mangle_ident(name);
        match scope.resolve(name) {
            VarKind::Plain => format!("{ident}.clone()"),
            VarKind::Cell | VarKind::Captured => format!("{ident}.borrow().clone()"),
            VarKind::Global => format!("rt.resolve_ident({})?", escape_rust_str(name)),
        }
    }

    fn emit_store_slot(&mut self, scope: &ScopeInfo, name: &str, val_expr: &str) {
        let ident = mangle_ident(name);
        match scope.resolve(name) {
            VarKind::Plain => self.line(&format!("{ident} = {val_expr};")),
            VarKind::Cell | VarKind::Captured => {
                self.line(&format!("*{ident}.borrow_mut() = {val_expr};"));
            }
            VarKind::Global => {
                self.line(&format!(
                    "rt.set_global({}, {val_expr});",
                    escape_rust_str(name)
                ));
            }
        }
    }

    fn emit_write_var(&mut self, scope: &ScopeInfo, name: &str, val_expr: &str, is_let: bool) {
        let ident = mangle_ident(name);
        let check_strict =
            scope.strict_vars.contains(name) && scope.resolve(name) != VarKind::Captured;
        if check_strict {
            let dflag = format!("{ident}_def");
            let cflag = format!("{ident}_const");
            let tflag = format!("{ident}_ty");
            let msg = escape_rust_str(&format!("cannot redefine constant '{name}'"));
            self.line(&format!(
                "if {dflag} && {cflag} {{ return Err(Flow::Err(String::from({msg}))); }}"
            ));
            if !is_let {
                self.line(&format!(
                    "if let Some(vy_exp) = {tflag} {{ rt.check_type(&({val_expr}), vy_exp, \"assignment\", {})?; }}",
                    escape_rust_str(name)
                ));
            }
        }
        self.emit_store_slot(scope, name, val_expr);
        if check_strict {
            let dflag = format!("{ident}_def");
            self.line(&format!("{dflag} = true;"));
        }
    }

    fn emit_closure_captures(
        &mut self,
        scope: &ScopeInfo,
        child_scope: &ScopeInfo,
    ) -> Vec<(String, String)> {
        let mut captures = Vec::new();
        for name in &scope.captured_cells {
            if child_scope.resolve(name) == VarKind::Captured {
                let ident = mangle_ident(name);
                let cap_tmp = self.fresh_tmp("cap");
                self.line(&format!("let {cap_tmp} = {ident}.clone();"));
                captures.push((ident, cap_tmp));
            }
        }
        captures
    }

    fn emit_fn_body_value(
        &mut self,
        params: &[(String, Option<String>)],
        ret_ty: &Option<String>,
        stmts: &[Stmt],
        parent_scope: &ScopeInfo,
        is_async: bool,
    ) -> Result<String, String> {
        let is_generator = stmts_have_yield(stmts);
        if is_async && is_generator {
            return Err("async generator functions are not yet supported".into());
        }
        let child_scope = self.build_scope(
            false,
            params,
            stmts,
            &parent_scope.captured_cells,
            &parent_scope.known_globals,
        );
        let captures = self.emit_closure_captures(parent_scope, &child_scope);
        let fn_var = self.fresh_tmp("fn");
        let spec_var = self.fresh_tmp("params");
        let mut spec_entries = Vec::with_capacity(params.len());
        for (p, ty) in params {
            let ty_str = match ty {
                Some(t) => format!("Some({})", escape_rust_str(t)),
                None => "None".to_string(),
            };
            spec_entries.push(format!("({}, {ty_str})", escape_rust_str(p)));
        }
        self.line(&format!("let {spec_var} = [{}];", spec_entries.join(", ")));
        self.line(&format!(
            "let {fn_var} = rt.reg_fn(Rc::new(move |rt: &mut Rt, mut vy_args: Vec<Val>, vy_kwargs: Vec<(String, Val)>| -> Result<Val, Flow> {{"
        ));
        self.indent += 1;
        for (ident, cap_tmp) in &captures {
            self.line(&format!("let {ident} = {cap_tmp}.clone();"));
        }
        self.line(&format!(
            "let vy_bound = rt.bind_params_kw(&mut vy_args, vy_kwargs, &{spec_var})?;"
        ));
        self.line("let mut vy_arg_it = vy_bound.into_iter();");
        for (p, ty) in params {
            let ident = mangle_ident(p);
            let next_expr = "vy_arg_it.next().unwrap_or(Val::Null)";
            match child_scope.resolve(p) {
                VarKind::Cell => {
                    self.line(&format!(
                        "let {ident}: Rc<RefCell<Val>> = Rc::new(RefCell::new({next_expr}));"
                    ));
                }
                VarKind::Plain => {
                    self.line(&format!("let mut {ident}: Val = {next_expr};"));
                }
                VarKind::Captured | VarKind::Global => {}
            }
            if child_scope.strict_vars.contains(p) {
                let dflag = format!("{ident}_def");
                let cflag = format!("{ident}_const");
                let tflag = format!("{ident}_ty");
                self.line(&format!("let mut {dflag}: bool = true;"));
                self.line(&format!("let mut {cflag}: bool = false;"));
                let ty_init = match ty.as_deref() {
                    Some(t) if t != "*" && t != "**" && t != "?default" => {
                        format!("Some({})", escape_rust_str(t))
                    }
                    _ => "None".to_string(),
                };
                self.line(&format!(
                    "let mut {tflag}: Option<&'static str> = {ty_init};"
                ));
            }
        }
        self.emit_var_decls(&child_scope, params);
        let prev_gen = self.in_generator;
        self.in_generator = is_generator;
        if is_generator {
            self.line("let mut vy_yielded: Vec<Val> = Vec::new();");
            self.line("let vy_body_res: Result<(), Flow> = (|| -> Result<(), Flow> {");
            self.indent += 1;
            let body_emit = self.emit_stmts(stmts, &child_scope);
            self.in_generator = prev_gen;
            body_emit?;
            self.line("Ok(())");
            self.indent -= 1;
            self.line("})();");
            self.line("match vy_body_res {");
            self.line("    Ok(()) | Err(Flow::Ret(_)) => {}");
            self.line(
                "    Err(Flow::Break) => return Err(Flow::Err(\"break outside loop\".into())),",
            );
            self.line(
                "    Err(Flow::Cont) => return Err(Flow::Err(\"continue outside loop\".into())),",
            );
            self.line("    Err(e) => return Err(e),");
            self.line("}");
            if let Some(expected) = ret_ty {
                if expected != "Generator" && expected != "Any" {
                    self.indent -= 1;
                    self.line("}));");
                    return Err(format!(
                        "generator function return type must be Generator or Any, got {expected}"
                    ));
                }
            }
            self.line("Ok(rt.new_generator(vy_yielded))");
        } else if is_async {
            self.line("let vy_async_res: Result<Val, Flow> = (|| -> Result<Val, Flow> {");
            self.indent += 1;
            self.line("let vy_body_res: Result<(), Flow> = (|| -> Result<(), Flow> {");
            self.indent += 1;
            let body_emit = self.emit_stmts(stmts, &child_scope);
            self.in_generator = prev_gen;
            body_emit?;
            self.line("Ok(())");
            self.indent -= 1;
            self.line("})();");
            self.line("let vy_ret_val = match vy_body_res {");
            self.line("    Ok(()) => Val::Null,");
            self.line("    Err(Flow::Ret(v)) => v,");
            self.line(
                "    Err(Flow::Break) => return Err(Flow::Err(\"break outside loop\".into())),",
            );
            self.line(
                "    Err(Flow::Cont) => return Err(Flow::Err(\"continue outside loop\".into())),",
            );
            self.line("    Err(e) => return Err(e),");
            self.line("};");
            if let Some(expected) = ret_ty {
                self.line(&format!(
                    "rt.check_type(&vy_ret_val, {}, \"return\", \"\")?;",
                    escape_rust_str(expected)
                ));
            }
            self.line("Ok(vy_ret_val)");
            self.indent -= 1;
            self.line("})();");
            self.line("let vy_task_res = match vy_async_res {");
            self.line("    Ok(v) => Ok(v),");
            self.line("    Err(Flow::Err(msg)) => Err(msg),");
            self.line(
                "    Err(_) => Err(String::from(\"unexpected control flow in async task\")),",
            );
            self.line("};");
            self.line("Ok(rt.store_task(vy_task_res))");
        } else {
            self.line("let vy_body_res: Result<(), Flow> = (|| -> Result<(), Flow> {");
            self.indent += 1;
            let body_emit = self.emit_stmts(stmts, &child_scope);
            self.in_generator = prev_gen;
            body_emit?;
            self.line("Ok(())");
            self.indent -= 1;
            self.line("})();");
            self.line("let vy_ret_val = match vy_body_res {");
            self.line("    Ok(()) => Val::Null,");
            self.line("    Err(Flow::Ret(v)) => v,");
            self.line(
                "    Err(Flow::Break) => return Err(Flow::Err(\"break outside loop\".into())),",
            );
            self.line(
                "    Err(Flow::Cont) => return Err(Flow::Err(\"continue outside loop\".into())),",
            );
            self.line("    Err(e) => return Err(e),");
            self.line("};");
            if let Some(expected) = ret_ty {
                self.line(&format!(
                    "rt.check_type(&vy_ret_val, {}, \"return\", \"\")?;",
                    escape_rust_str(expected)
                ));
            }
            self.line("Ok(vy_ret_val)");
        }
        self.indent -= 1;
        self.line("}));");
        let p_strs: Vec<String> = params
            .iter()
            .take_while(|(_, ty)| !matches!(ty.as_deref(), Some("*") | Some("**")))
            .map(|(p, _)| escape_rust_str(p))
            .collect();
        self.line(&format!(
            "rt.reg_fn_params(&{fn_var}, &[{}]);",
            p_strs.join(", ")
        ));
        Ok(fn_var)
    }

    fn emit_call_ext_args(
        &mut self,
        args: &[CallArg],
        scope: &ScopeInfo,
    ) -> Result<(String, String), String> {
        let pos_v = self.fresh_tmp("pos");
        let kw_v = self.fresh_tmp("kw");
        self.line(&format!("let mut {pos_v}: Vec<Val> = Vec::new();"));
        self.line(&format!("let mut {kw_v}: Vec<(String, Val)> = Vec::new();"));
        for arg in args {
            match arg {
                CallArg::Pos(e) => {
                    let ev = self.emit_expr(e, scope)?;
                    self.line(&format!("{pos_v}.push({ev});"));
                }
                CallArg::Star(e) => {
                    let ev = self.emit_expr(e, scope)?;
                    self.line(&format!("{pos_v}.extend(rt.to_iter({ev})?);"));
                }
                CallArg::Kw(k, e) => {
                    let ev = self.emit_expr(e, scope)?;
                    self.line(&format!(
                        "{kw_v}.push((String::from({}), {ev}));",
                        escape_rust_str(k)
                    ));
                }
                CallArg::KwStar(e) => {
                    let ev = self.emit_expr(e, scope)?;
                    self.line(&format!("rt.extend_kwargs({ev}, &mut {kw_v})?;"));
                }
            }
        }
        Ok((pos_v, kw_v))
    }

    fn emit_expr(&mut self, expr: &Expr, scope: &ScopeInfo) -> Result<String, String> {
        let tmp = self.fresh_tmp("e");
        match expr {
            Expr::Num(n) => {
                self.line(&format!("let {tmp} = Val::Num({n:?}f64);"));
            }
            Expr::Str(s) => {
                self.line(&format!(
                    "let {tmp} = Val::Str(String::from({}));",
                    escape_rust_str(s)
                ));
            }
            Expr::Bool(b) => {
                self.line(&format!("let {tmp} = Val::Bool({b});"));
            }
            Expr::Null => {
                self.line(&format!("let {tmp} = Val::Null;"));
            }
            Expr::Var(n) => {
                let read = self.emit_read_var(scope, n);
                self.line(&format!("let {tmp} = {read};"));
            }
            Expr::List(xs) => {
                let mut item_vars = Vec::with_capacity(xs.len());
                for x in xs {
                    item_vars.push(self.emit_expr(x, scope)?);
                }
                self.line(&format!(
                    "let {tmp} = Val::List(vec![{}]);",
                    item_vars.join(", ")
                ));
            }
            Expr::Map(pairs) => {
                let mut pair_exprs = Vec::with_capacity(pairs.len());
                for (k, v) in pairs {
                    let kv = self.emit_expr(k, scope)?;
                    let vv = self.emit_expr(v, scope)?;
                    pair_exprs.push(format!("({kv}, {vv})"));
                }
                self.line(&format!(
                    "let {tmp} = Val::Map(vec![{}]);",
                    pair_exprs.join(", ")
                ));
            }
            Expr::Index(a, b) => {
                let av = self.emit_expr(a, scope)?;
                let bv = self.emit_expr(b, scope)?;
                self.line(&format!("let {tmp} = rt.val_index({av}, {bv})?;"));
            }
            Expr::Member(a, field) => {
                let av = self.emit_expr(a, scope)?;
                self.line(&format!(
                    "let {tmp} = rt.get_member({av}, {})?;",
                    escape_rust_str(field)
                ));
            }
            Expr::MethodCall(recv, method, args) => {
                let rv = self.emit_expr(recv, scope)?;
                let mut arg_vars = Vec::with_capacity(args.len());
                for a in args {
                    arg_vars.push(self.emit_expr(a, scope)?);
                }
                self.line(&format!(
                    "let {tmp} = rt.call_method({rv}, {}, vec![{}])?;",
                    escape_rust_str(method),
                    arg_vars.join(", ")
                ));
            }
            Expr::Unary(op, inner) => {
                let v = self.emit_expr(inner, scope)?;
                if op == "-" {
                    self.line(&format!("let {tmp} = rt.neg({v})?;"));
                } else {
                    self.line(&format!("let {tmp} = Val::Bool(!({v}).truthy());"));
                }
            }
            Expr::Bin(a, op, b) => {
                if op == "and" {
                    let av = self.emit_expr(a, scope)?;
                    self.line(&format!("let {tmp} = if ({av}).truthy() {{"));
                    self.indent += 1;
                    let bv = self.emit_expr(b, scope)?;
                    self.line(&format!("Val::Bool(({bv}).truthy())"));
                    self.indent -= 1;
                    self.line("} else { Val::Bool(false) };");
                } else if op == "or" {
                    let av = self.emit_expr(a, scope)?;
                    self.line(&format!(
                        "let {tmp} = if ({av}).truthy() {{ Val::Bool(true) }} else {{"
                    ));
                    self.indent += 1;
                    let bv = self.emit_expr(b, scope)?;
                    self.line(&format!("Val::Bool(({bv}).truthy())"));
                    self.indent -= 1;
                    self.line("};");
                } else {
                    let av = self.emit_expr(a, scope)?;
                    let bv = self.emit_expr(b, scope)?;
                    let helper = match op.as_str() {
                        "+" => "bin_add",
                        "-" => "bin_sub",
                        "*" => "bin_mul",
                        "/" => "bin_div",
                        "%" => "bin_mod",
                        "**" => "bin_pow",
                        "==" => "bin_eq",
                        "!=" => "bin_ne",
                        "<" => "bin_lt",
                        "<=" => "bin_le",
                        ">" => "bin_gt",
                        ">=" => "bin_ge",
                        other => return Err(format!("unsupported binary operator '{other}'")),
                    };
                    self.line(&format!("let {tmp} = rt.{helper}({av}, {bv})?;"));
                }
            }
            Expr::Ternary(cond, then_e, else_e) => {
                let cv = self.emit_expr(cond, scope)?;
                self.line(&format!("let {tmp} = if ({cv}).truthy() {{"));
                self.indent += 1;
                let tv = self.emit_expr(then_e, scope)?;
                self.line(&tv);
                self.indent -= 1;
                self.line("} else {");
                self.indent += 1;
                let ev = self.emit_expr(else_e, scope)?;
                self.line(&ev);
                self.indent -= 1;
                self.line("};");
            }
            Expr::Call(name, args) => {
                let mut arg_vars = Vec::with_capacity(args.len());
                for a in args {
                    arg_vars.push(self.emit_expr(a, scope)?);
                }
                let local_opt = match scope.resolve(name) {
                    VarKind::Plain | VarKind::Cell | VarKind::Captured => {
                        format!("Some({})", self.emit_read_var(scope, name))
                    }
                    VarKind::Global => "None".to_string(),
                };
                self.line(&format!(
                    "let {tmp} = rt.call_target({}, {local_opt}, vec![{}])?;",
                    escape_rust_str(name),
                    arg_vars.join(", ")
                ));
            }
            Expr::Lambda(params, ret_ty, body) => {
                let fn_v = self.emit_fn_body_value(params, ret_ty, body, scope, false)?;
                self.line(&format!("let {tmp} = {fn_v};"));
            }
            Expr::CallExpr(callee, args) => {
                let cv = self.emit_expr(callee, scope)?;
                let mut arg_vars = Vec::with_capacity(args.len());
                for a in args {
                    arg_vars.push(self.emit_expr(a, scope)?);
                }
                self.line(&format!(
                    "let {tmp} = rt.call_val(&{cv}, vec![{}])?;",
                    arg_vars.join(", ")
                ));
            }
            Expr::CallExt {
                callee,
                method,
                args,
            } => {
                if let Some(mname) = method {
                    let rv = self.emit_expr(callee, scope)?;
                    let (pos_v, kw_v) = self.emit_call_ext_args(args, scope)?;
                    self.line(&format!(
                        "let {tmp} = rt.call_method_kw({rv}, {}, {pos_v}, {kw_v})?;",
                        escape_rust_str(mname)
                    ));
                } else if let Expr::Var(name) = callee.as_ref() {
                    let (pos_v, kw_v) = self.emit_call_ext_args(args, scope)?;
                    let local_opt = match scope.resolve(name) {
                        VarKind::Plain | VarKind::Cell | VarKind::Captured => {
                            format!("Some({})", self.emit_read_var(scope, name))
                        }
                        VarKind::Global => "None".to_string(),
                    };
                    self.line(&format!(
                        "let {tmp} = rt.call_target_kw({}, {local_opt}, {pos_v}, {kw_v})?;",
                        escape_rust_str(name)
                    ));
                } else {
                    let cv = self.emit_expr(callee, scope)?;
                    let (pos_v, kw_v) = self.emit_call_ext_args(args, scope)?;
                    self.line(&format!(
                        "let {tmp} = rt.call_val_kw(&{cv}, {pos_v}, {kw_v})?;"
                    ));
                }
            }
            Expr::ChainedCmp(first, rest) => {
                let lbl = self.fresh_tmp("cmp");
                let first_v = self.emit_expr(first, scope)?;
                self.line(&format!("let {tmp} = '{lbl}: {{"));
                self.indent += 1;
                let prev_v = self.fresh_tmp("lhs");
                self.line(&format!("let mut {prev_v} = {first_v};"));
                for (op, rhs) in rest {
                    let rv = self.emit_expr(rhs, scope)?;
                    let helper = match op.as_str() {
                        "==" => "bin_eq",
                        "!=" => "bin_ne",
                        "<" => "bin_lt",
                        "<=" => "bin_le",
                        ">" => "bin_gt",
                        ">=" => "bin_ge",
                        other => return Err(format!("unsupported comparison operator '{other}'")),
                    };
                    let chk = self.fresh_tmp("chk");
                    self.line(&format!(
                        "let {chk} = rt.{helper}({prev_v}, {rv}.clone())?;"
                    ));
                    self.line(&format!(
                        "if !({chk}).truthy() {{ break '{lbl} Val::Bool(false); }}"
                    ));
                    self.line(&format!("{prev_v} = {rv};"));
                }
                self.line("Val::Bool(true)");
                self.indent -= 1;
                self.line("};");
            }
            Expr::ListComp(map_expr, var, iter_expr, cond_opt) => {
                let iv = self.emit_expr(iter_expr, scope)?;
                let mut nested_refs = BTreeSet::new();
                collect_nested_refs_expr(map_expr, &mut nested_refs);
                if let Some(c) = cond_opt {
                    collect_nested_refs_expr(c, &mut nested_refs);
                }
                let mut inner_scope = scope.clone();
                let var_kind = if nested_refs.contains(var) {
                    inner_scope.captured_cells.insert(var.clone());
                    VarKind::Cell
                } else {
                    VarKind::Plain
                };
                inner_scope.vars.insert(var.clone(), var_kind);
                let vec_tmp = self.fresh_tmp("lc");
                self.line(&format!("let {tmp} = {{"));
                self.indent += 1;
                self.line(&format!("let vy_iter = rt.to_iter({iv})?;"));
                self.line(&format!(
                    "let mut {vec_tmp}: Vec<Val> = Vec::with_capacity(vy_iter.len());"
                ));
                self.line("for vy_item in vy_iter {");
                self.indent += 1;
                let var_ident = mangle_ident(var);
                match var_kind {
                    VarKind::Cell => self.line(&format!(
                        "let {var_ident}: Rc<RefCell<Val>> = Rc::new(RefCell::new(vy_item));"
                    )),
                    VarKind::Plain => self.line(&format!("let {var_ident}: Val = vy_item;")),
                    VarKind::Captured | VarKind::Global => {}
                }
                if let Some(cond) = cond_opt {
                    let cv = self.emit_expr(cond, &inner_scope)?;
                    self.line(&format!("if ({cv}).truthy() {{"));
                    self.indent += 1;
                    let mv = self.emit_expr(map_expr, &inner_scope)?;
                    self.line(&format!("{vec_tmp}.push({mv});"));
                    self.indent -= 1;
                    self.line("}");
                } else {
                    let mv = self.emit_expr(map_expr, &inner_scope)?;
                    self.line(&format!("{vec_tmp}.push({mv});"));
                }
                self.indent -= 1;
                self.line("}");
                self.line(&format!("Val::List({vec_tmp})"));
                self.indent -= 1;
                self.line("};");
            }
            Expr::Await(inner) => {
                let iv = self.emit_expr(inner, scope)?;
                self.line(&format!("let {tmp} = rt.await_val({iv})?;"));
            }
        }
        Ok(tmp)
    }

    fn emit_stmts(&mut self, stmts: &[Stmt], scope: &ScopeInfo) -> Result<(), String> {
        for stmt in stmts {
            self.emit_stmt(stmt, scope)?;
        }
        Ok(())
    }

    fn emit_stmt(&mut self, stmt: &Stmt, scope: &ScopeInfo) -> Result<(), String> {
        match stmt {
            Stmt::SourceLine(_) => {}
            Stmt::Let(name, ty_opt, expr, is_const) => {
                let val_tmp = self.emit_expr(expr, scope)?;
                let ident = mangle_ident(name);
                let is_strict =
                    scope.strict_vars.contains(name) && scope.resolve(name) != VarKind::Captured;
                if is_strict {
                    let dflag = format!("{ident}_def");
                    let cflag = format!("{ident}_const");
                    let tflag = format!("{ident}_ty");
                    let redef_msg = escape_rust_str(&format!("cannot redefine constant '{name}'"));
                    self.line(&format!(
                        "if {dflag} && {cflag} {{ return Err(Flow::Err(String::from({redef_msg}))); }}"
                    ));
                    if *is_const {
                        let dup_msg = escape_rust_str(&format!("name '{name}' already defined"));
                        self.line(&format!(
                            "if {dflag} {{ return Err(Flow::Err(String::from({dup_msg}))); }}"
                        ));
                    }
                    if let Some(expected) = ty_opt {
                        self.line(&format!(
                            "rt.check_type(&{val_tmp}, {}, \"declaration\", {})?;",
                            escape_rust_str(expected),
                            escape_rust_str(name)
                        ));
                    }
                    self.emit_store_slot(scope, name, &val_tmp);
                    self.line(&format!("{dflag} = true;"));
                    if *is_const {
                        self.line(&format!("{cflag} = true;"));
                    }
                    if let Some(expected) = ty_opt {
                        self.line(&format!("{tflag} = Some({});", escape_rust_str(expected)));
                    }
                } else {
                    self.emit_write_var(scope, name, &val_tmp, true);
                }
            }
            Stmt::Assign(name, expr) => {
                let val_tmp = self.emit_expr(expr, scope)?;
                let is_strict =
                    scope.strict_vars.contains(name) && scope.resolve(name) != VarKind::Captured;
                if is_strict {
                    let ident = mangle_ident(name);
                    let cflag = format!("{ident}_const");
                    let tflag = format!("{ident}_ty");
                    let assign_msg =
                        escape_rust_str(&format!("cannot assign to constant '{name}'"));
                    self.line(&format!(
                        "if {cflag} {{ return Err(Flow::Err(String::from({assign_msg}))); }}"
                    ));
                    self.line(&format!(
                        "if let Some(vy_exp) = {tflag} {{ rt.check_type(&{val_tmp}, vy_exp, \"assignment\", {})?; }}",
                        escape_rust_str(name)
                    ));
                    self.emit_store_slot(scope, name, &val_tmp);
                } else {
                    self.emit_write_var(scope, name, &val_tmp, false);
                }
            }
            Stmt::Expr(expr) => {
                let _ = self.emit_expr(expr, scope)?;
            }
            Stmt::If(cond, then_b, else_b) => {
                let cv = self.emit_expr(cond, scope)?;
                self.line(&format!("if ({cv}).truthy() {{"));
                self.indent += 1;
                self.emit_stmts(then_b, scope)?;
                self.indent -= 1;
                if !else_b.is_empty() {
                    self.line("} else {");
                    self.indent += 1;
                    self.emit_stmts(else_b, scope)?;
                    self.indent -= 1;
                }
                self.line("}");
            }
            Stmt::While(cond, body) => {
                self.line("loop {");
                self.indent += 1;
                let cv = self.emit_expr(cond, scope)?;
                self.line(&format!("if !({cv}).truthy() {{ break; }}"));
                self.line("let vy_loop_step: Result<(), Flow> = (|| -> Result<(), Flow> {");
                self.indent += 1;
                self.emit_stmts(body, scope)?;
                self.line("Ok(())");
                self.indent -= 1;
                self.line("})();");
                self.line("match vy_loop_step {");
                self.line("    Ok(()) | Err(Flow::Cont) => {}");
                self.line("    Err(Flow::Break) => break,");
                self.line("    Err(e) => return Err(e),");
                self.line("}");
                self.indent -= 1;
                self.line("}");
            }
            Stmt::ForRange(var, start_e, end_e, body) => {
                let sv = self.emit_expr(start_e, scope)?;
                self.emit_write_var(scope, var, &sv, true);
                self.line("loop {");
                self.indent += 1;
                let cur = self.emit_read_var(scope, var);
                let ev = self.emit_expr(end_e, scope)?;
                self.line(&format!(
                    "if !rt.bin_lt({cur}, {ev})?.truthy() {{ break; }}"
                ));
                self.line("let vy_loop_step: Result<(), Flow> = (|| -> Result<(), Flow> {");
                self.indent += 1;
                self.emit_stmts(body, scope)?;
                self.line("Ok(())");
                self.indent -= 1;
                self.line("})();");
                self.line("match vy_loop_step {");
                self.line("    Ok(()) | Err(Flow::Cont) => {}");
                self.line("    Err(Flow::Break) => break,");
                self.line("    Err(e) => return Err(e),");
                self.line("}");
                let cur2 = self.emit_read_var(scope, var);
                let next_tmp = self.fresh_tmp("next");
                self.line(&format!(
                    "let {next_tmp} = rt.bin_add({cur2}, Val::Num(1.0))?;"
                ));
                self.emit_write_var(scope, var, &next_tmp, false);
                self.indent -= 1;
                self.line("}");
            }
            Stmt::ForEach(var, iter_e, body) => {
                let iv = self.emit_expr(iter_e, scope)?;
                let items_tmp = self.fresh_tmp("items");
                self.line(&format!("let {items_tmp} = rt.to_iter({iv})?;"));
                self.line(&format!("for vy_elem in {items_tmp} {{"));
                self.indent += 1;
                self.emit_write_var(scope, var, "vy_elem", true);
                self.line("let vy_loop_step: Result<(), Flow> = (|| -> Result<(), Flow> {");
                self.indent += 1;
                self.emit_stmts(body, scope)?;
                self.line("Ok(())");
                self.indent -= 1;
                self.line("})();");
                self.line("match vy_loop_step {");
                self.line("    Ok(()) | Err(Flow::Cont) => {}");
                self.line("    Err(Flow::Break) => break,");
                self.line("    Err(e) => return Err(e),");
                self.line("}");
                self.indent -= 1;
                self.line("}");
            }
            Stmt::Try(body, err_var, handler, finally) => {
                let res_tmp = self.fresh_tmp("try");
                self.line(&format!(
                    "let mut {res_tmp}: Result<(), Flow> = (|| -> Result<(), Flow> {{"
                ));
                self.indent += 1;
                self.emit_stmts(body, scope)?;
                self.line("Ok(())");
                self.indent -= 1;
                self.line("})();");
                self.line(&format!("if let Err(Flow::Err(vy_err_msg)) = {res_tmp} {{"));
                self.indent += 1;
                self.emit_write_var(scope, err_var, "Val::Str(vy_err_msg)", true);
                self.line(&format!("{res_tmp} = (|| -> Result<(), Flow> {{"));
                self.indent += 1;
                self.emit_stmts(handler, scope)?;
                self.line("Ok(())");
                self.indent -= 1;
                self.line("})();");
                self.indent -= 1;
                self.line("}");
                if !finally.is_empty() {
                    let fin_tmp = self.fresh_tmp("fin");
                    self.line(&format!(
                        "let {fin_tmp}: Result<(), Flow> = (|| -> Result<(), Flow> {{"
                    ));
                    self.indent += 1;
                    self.emit_stmts(finally, scope)?;
                    self.line("Ok(())");
                    self.indent -= 1;
                    self.line("})();");
                    self.line(&format!("{fin_tmp}?;"));
                }
                self.line(&format!("{res_tmp}?;"));
            }
            Stmt::Throw(expr) => {
                let val_tmp = self.emit_expr(expr, scope)?;
                self.line(&format!("return Err(Flow::Err(({val_tmp}).show()));"));
            }
            Stmt::Fn(name, params, ret_ty, body) => {
                let fn_v = self.emit_fn_body_value(params, ret_ty, body, scope, false)?;
                if scope.is_top {
                    self.line(&format!(
                        "if let Val::Fn(vy_fid) = {fn_v} {{ rt.named_fns.insert(String::from({}), vy_fid); }}",
                        escape_rust_str(name)
                    ));
                } else {
                    self.emit_write_var(scope, name, &fn_v, true);
                }
            }
            Stmt::AsyncFn(name, params, ret_ty, body) => {
                let fn_v = self.emit_fn_body_value(params, ret_ty, body, scope, true)?;
                if scope.is_top {
                    self.line(&format!(
                        "if let Val::Fn(vy_fid) = {fn_v} {{ rt.named_fns.insert(String::from({}), vy_fid); }}",
                        escape_rust_str(name)
                    ));
                } else {
                    self.emit_write_var(scope, name, &fn_v, true);
                }
            }
            Stmt::DecoratedFn(decs, name, params, ret_ty, body) => {
                let mut dec_tmps = Vec::with_capacity(decs.len());
                for d in decs {
                    if !crate::pipeline::is_static_effect_decorator(d) {
                        dec_tmps.push(self.emit_expr(d, scope)?);
                    }
                }
                let mut wrapped = self.emit_fn_body_value(params, ret_ty, body, scope, false)?;
                for d_tmp in dec_tmps.iter().rev() {
                    let nxt = self.fresh_tmp("dec");
                    self.line(&format!(
                        "let {nxt} = rt.call_val(&{d_tmp}, vec![{wrapped}])?;"
                    ));
                    wrapped = nxt;
                }
                if scope.is_top {
                    self.line(&format!(
                        "rt.set_global({}, {wrapped});",
                        escape_rust_str(name)
                    ));
                } else {
                    self.emit_write_var(scope, name, &wrapped, true);
                }
            }
            Stmt::Class(name, bases, metaclass, decorators, body) => {
                let mut dec_tmps = Vec::with_capacity(decorators.len());
                for d in decorators {
                    dec_tmps.push(self.emit_expr(d, scope)?);
                }
                let mut base_vars = Vec::with_capacity(bases.len());
                for b in bases {
                    base_vars.push(self.emit_expr(b, scope)?);
                }
                let meta_v = match metaclass {
                    Some(m) => self.emit_expr(m, scope)?,
                    None => "Val::Null".to_string(),
                };
                let mut defined: Vec<String> = Vec::new();
                let mut push_def = |n: &str| {
                    if !defined.iter().any(|x| x == n) {
                        defined.push(n.to_string());
                    }
                };
                let mut ns_stmts = Vec::with_capacity(body.len() + 1);
                for item in body {
                    match item {
                        Stmt::Let(n, ..)
                        | Stmt::Fn(n, ..)
                        | Stmt::AsyncFn(n, ..)
                        | Stmt::DecoratedFn(_, n, ..)
                        | Stmt::Class(n, ..) => {
                            push_def(n);
                            ns_stmts.push(item.clone());
                        }
                        Stmt::Assign(n, e) => {
                            push_def(n);
                            ns_stmts.push(Stmt::Let(n.clone(), None, e.clone(), false));
                        }
                        _ => ns_stmts.push(item.clone()),
                    }
                }
                let map_pairs: Vec<(Expr, Expr)> = defined
                    .iter()
                    .map(|n| (Expr::Str(n.clone()), Expr::Var(n.clone())))
                    .collect();
                ns_stmts.push(Stmt::Return(Some(Expr::Map(map_pairs))));
                let body_fn = self.emit_fn_body_value(&[], &None, &ns_stmts, scope, false)?;
                let mut cls_val = self.fresh_tmp("cls_val");
                self.line(&format!(
                    "let {cls_val} = rt.build_class(String::from({}), vec![{}], {meta_v}, {body_fn})?;",
                    escape_rust_str(name),
                    base_vars.join(", ")
                ));
                for d_tmp in dec_tmps.iter().rev() {
                    let nxt = self.fresh_tmp("cls_dec");
                    self.line(&format!(
                        "let {nxt} = rt.call_val(&{d_tmp}, vec![{cls_val}])?;"
                    ));
                    cls_val = nxt;
                }
                if scope.is_top {
                    self.line(&format!(
                        "rt.set_global({}, {cls_val});",
                        escape_rust_str(name)
                    ));
                } else {
                    self.emit_write_var(scope, name, &cls_val, true);
                }
            }
            Stmt::SetMember(obj, field, expr) => {
                let ov = self.emit_expr(obj, scope)?;
                let vv = self.emit_expr(expr, scope)?;
                self.line(&format!(
                    "rt.set_member({ov}, {}, {vv})?;",
                    escape_rust_str(field)
                ));
            }
            Stmt::SetIndex(target, idx, val) => match target {
                Expr::Var(name) => {
                    let cur = self.emit_read_var(scope, name);
                    let iv = self.emit_expr(idx, scope)?;
                    let vv = self.emit_expr(val, scope)?;
                    let upd = self.fresh_tmp("setidx");
                    self.line(&format!(
                        "let {upd} = rt.val_set_index({cur}, {iv}, {vv})?;"
                    ));
                    self.emit_write_var(scope, name, &upd, false);
                }
                Expr::Member(obj, field) => {
                    let ov = self.emit_expr(obj, scope)?;
                    let cur = self.fresh_tmp("cur");
                    self.line(&format!(
                        "let {cur} = rt.get_member({ov}.clone(), {})?;",
                        escape_rust_str(field)
                    ));
                    let iv = self.emit_expr(idx, scope)?;
                    let vv = self.emit_expr(val, scope)?;
                    let upd = self.fresh_tmp("setidx");
                    self.line(&format!(
                        "let {upd} = rt.val_set_index({cur}, {iv}, {vv})?;"
                    ));
                    self.line(&format!(
                        "rt.set_member({ov}, {}, {upd})?;",
                        escape_rust_str(field)
                    ));
                }
                Expr::Index(outer_target, outer_idx) => {
                    if let Expr::Var(name) = outer_target.as_ref() {
                        let outer_cur = self.emit_read_var(scope, name);
                        let oiv = self.emit_expr(outer_idx, scope)?;
                        let inner_cur = self.fresh_tmp("inner");
                        self.line(&format!(
                            "let {inner_cur} = rt.val_index({outer_cur}.clone(), {oiv}.clone())?;"
                        ));
                        let iiv = self.emit_expr(idx, scope)?;
                        let vv = self.emit_expr(val, scope)?;
                        let inner_upd = self.fresh_tmp("in_upd");
                        self.line(&format!(
                            "let {inner_upd} = rt.val_set_index({inner_cur}, {iiv}, {vv})?;"
                        ));
                        let outer_upd = self.fresh_tmp("out_upd");
                        self.line(&format!(
                            "let {outer_upd} = rt.val_set_index({outer_cur}, {oiv}, {inner_upd})?;"
                        ));
                        self.emit_write_var(scope, name, &outer_upd, false);
                    } else {
                        return Err("unsupported nested index assignment target".into());
                    }
                }
                _ => return Err("unsupported index assignment target".into()),
            },
            Stmt::Match(subj, arms) => {
                let sv = self.emit_expr(subj, scope)?;
                let lbl = self.fresh_tmp("match");
                self.line(&format!("'{lbl}: {{"));
                self.indent += 1;
                let mut wildcard_seen = false;
                for (pat, guard, body) in arms {
                    if wildcard_seen {
                        return Err("match cases after wildcard '_' are unreachable".into());
                    }
                    if guard.is_none() && Self::pattern_is_irrefutable(pat) {
                        wildcard_seen = true;
                    }
                    let arm_lbl = self.fresh_tmp("pat_arm");
                    self.line(&format!("'{arm_lbl}: {{"));
                    self.indent += 1;
                    let mut bindings = Vec::new();
                    self.emit_aot_pattern(&sv, pat, scope, &arm_lbl, &mut bindings)?;
                    for (var_name, tmp_var) in bindings {
                        let clone_expr = format!("{tmp_var}.clone()");
                        self.emit_write_var(scope, &var_name, &clone_expr, true);
                    }
                    if let Some(g) = guard {
                        let gv = self.emit_expr(g, scope)?;
                        self.line(&format!("if !({gv}).truthy() {{ break '{arm_lbl}; }}"));
                    }
                    self.emit_stmts(body, scope)?;
                    self.line(&format!("break '{lbl};"));
                    self.indent -= 1;
                    self.line("}");
                }
                self.indent -= 1;
                self.line("}");
            }
            Stmt::With(resource, name, body) => {
                let rv = self.emit_expr(resource, scope)?;
                let entered = self.fresh_tmp("with_val");
                self.line(&format!(
                    "let {entered} = rt.call_method({rv}.clone(), \"enter\", vec![])?;"
                ));
                self.emit_write_var(scope, name, &entered, true);
                let body_res = self.fresh_tmp("with_res");
                self.line(&format!(
                    "let {body_res}: Result<(), Flow> = (|| -> Result<(), Flow> {{"
                ));
                self.indent += 1;
                self.emit_stmts(body, scope)?;
                self.line("Ok(())");
                self.indent -= 1;
                self.line("})();");
                self.line(&format!("let _ = rt.call_method({rv}, \"exit\", vec![])?;"));
                self.line(&format!("{body_res}?;"));
            }
            Stmt::Return(Some(expr)) => {
                let vv = self.emit_expr(expr, scope)?;
                self.line(&format!("return Err(Flow::Ret({vv}));"));
            }
            Stmt::Return(None) => self.line("return Err(Flow::Ret(Val::Null));"),
            Stmt::Break => self.line("return Err(Flow::Break);"),
            Stmt::Continue => self.line("return Err(Flow::Cont);"),
            Stmt::Yield(expr) => {
                if !self.in_generator {
                    return Err("yield statement used outside generator function".into());
                }
                let yv = self.emit_expr(expr, scope)?;
                self.line(&format!("vy_yielded.push({yv});"));
            }
        }
        Ok(())
    }

    fn pattern_is_irrefutable(pat: &Pattern) -> bool {
        match pat {
            Pattern::Wildcard | Pattern::Capture(_) => true,
            Pattern::As(inner, _) => Self::pattern_is_irrefutable(inner),
            Pattern::Or(alts) => alts.iter().any(Self::pattern_is_irrefutable),
            _ => false,
        }
    }

    fn collect_pattern_bound_vars_ordered(
        pat: &Pattern,
        out: &mut Vec<String>,
        seen: &mut BTreeSet<String>,
    ) {
        match pat {
            Pattern::Wildcard | Pattern::Literal(_) => {}
            Pattern::Capture(name) => {
                if seen.insert(name.clone()) {
                    out.push(name.clone());
                }
            }
            Pattern::Or(alts) => {
                if let Some(first) = alts.first() {
                    Self::collect_pattern_bound_vars_ordered(first, out, seen);
                }
            }
            Pattern::As(inner, name) => {
                Self::collect_pattern_bound_vars_ordered(inner, out, seen);
                if seen.insert(name.clone()) {
                    out.push(name.clone());
                }
            }
            Pattern::Seq(elems) => {
                for el in elems {
                    match el {
                        SeqElem::Pat(p) => Self::collect_pattern_bound_vars_ordered(p, out, seen),
                        SeqElem::Star(Some(name)) => {
                            if seen.insert(name.clone()) {
                                out.push(name.clone());
                            }
                        }
                        SeqElem::Star(None) => {}
                    }
                }
            }
            Pattern::Map { entries, rest } => {
                for (_, p) in entries {
                    Self::collect_pattern_bound_vars_ordered(p, out, seen);
                }
                if let Some(r) = rest {
                    if seen.insert(r.clone()) {
                        out.push(r.clone());
                    }
                }
            }
            Pattern::Class { pos, kw, .. } => {
                for p in pos {
                    Self::collect_pattern_bound_vars_ordered(p, out, seen);
                }
                for (_, p) in kw {
                    Self::collect_pattern_bound_vars_ordered(p, out, seen);
                }
            }
        }
    }

    fn emit_aot_pattern(
        &mut self,
        subj_var: &str,
        pat: &Pattern,
        scope: &ScopeInfo,
        fail_lbl: &str,
        bindings: &mut Vec<(String, String)>,
    ) -> Result<(), String> {
        match pat {
            Pattern::Wildcard => {}
            Pattern::Capture(name) => {
                let tmp = self.fresh_tmp("pat_cap");
                self.line(&format!("let {tmp} = {subj_var}.clone();"));
                bindings.push((name.clone(), tmp));
            }
            Pattern::As(inner, name) => {
                self.emit_aot_pattern(subj_var, inner, scope, fail_lbl, bindings)?;
                let tmp = self.fresh_tmp("pat_as");
                self.line(&format!("let {tmp} = {subj_var}.clone();"));
                bindings.push((name.clone(), tmp));
            }
            Pattern::Literal(expr) => {
                let pv = self.emit_expr(expr, scope)?;
                self.line(&format!("if {subj_var} != {pv} {{ break '{fail_lbl}; }}"));
            }
            Pattern::Or(alts) => {
                let mut bound_names = Vec::new();
                let mut seen = BTreeSet::new();
                Self::collect_pattern_bound_vars_ordered(pat, &mut bound_names, &mut seen);
                let mut shared_map: HashMap<String, String> = HashMap::new();
                for name in bound_names {
                    let st = self.fresh_tmp("pat_or");
                    self.line(&format!("let mut {st}: Val = Val::Null;"));
                    shared_map.insert(name.clone(), st.clone());
                    bindings.push((name, st));
                }
                let or_ok = self.fresh_tmp("pat_or_ok");
                self.line(&format!("'{or_ok}: {{"));
                self.indent += 1;
                for alt in alts {
                    let alt_lbl = self.fresh_tmp("pat_alt");
                    self.line(&format!("'{alt_lbl}: {{"));
                    self.indent += 1;
                    let mut alt_binds = Vec::new();
                    self.emit_aot_pattern(subj_var, alt, scope, &alt_lbl, &mut alt_binds)?;
                    for (bname, btmp) in &alt_binds {
                        if let Some(shared) = shared_map.get(bname) {
                            self.line(&format!("{shared} = {btmp}.clone();"));
                        }
                    }
                    self.line(&format!("break '{or_ok};"));
                    self.indent -= 1;
                    self.line("}");
                }
                self.line(&format!("break '{fail_lbl};"));
                self.indent -= 1;
                self.line("}");
            }
            Pattern::Seq(elems) => {
                let star_pos = elems.iter().position(|e| matches!(e, SeqElem::Star(_)));
                let has_star = star_pos.is_some();
                let (before, after) = match star_pos {
                    Some(pos) => (pos, elems.len() - pos - 1),
                    None => (elems.len(), 0),
                };
                let parts_v = self.fresh_tmp("pat_seq");
                self.line(&format!(
                    "let Some({parts_v}) = rt.pat_seq_split(&{subj_var}, {before}, {has_star}, {after}) else {{ break '{fail_lbl}; }};"
                ));
                for (idx, elem) in elems.iter().enumerate() {
                    match elem {
                        SeqElem::Star(None) => {}
                        SeqElem::Star(Some(name)) => {
                            let el_v = self.fresh_tmp("pat_star");
                            self.line(&format!("let {el_v} = {parts_v}[{idx}].clone();"));
                            bindings.push((name.clone(), el_v));
                        }
                        SeqElem::Pat(sub_pat) => {
                            if !matches!(sub_pat, Pattern::Wildcard) {
                                let el_v = self.fresh_tmp("pat_el");
                                self.line(&format!("let {el_v} = {parts_v}[{idx}].clone();"));
                                self.emit_aot_pattern(&el_v, sub_pat, scope, fail_lbl, bindings)?;
                            }
                        }
                    }
                }
            }
            Pattern::Map { entries, rest } => {
                self.line(&format!(
                    "if !matches!(&{subj_var}, Val::Map(_)) {{ break '{fail_lbl}; }}"
                ));
                let mut excl_vars = Vec::new();
                for (kexpr, vpat) in entries {
                    let kv = self.emit_expr(kexpr, scope)?;
                    let mv = self.fresh_tmp("pat_mv");
                    if rest.is_some() {
                        let kt = self.fresh_tmp("pat_mk");
                        self.line(&format!("let {kt} = {kv}.clone();"));
                        self.line(&format!(
                            "let Some({mv}) = rt.pat_map_get(&{subj_var}, &{kt}) else {{ break '{fail_lbl}; }};"
                        ));
                        excl_vars.push(kt);
                    } else {
                        self.line(&format!(
                            "let Some({mv}) = rt.pat_map_get(&{subj_var}, &{kv}) else {{ break '{fail_lbl}; }};"
                        ));
                    }
                    if !matches!(vpat, Pattern::Wildcard) {
                        self.emit_aot_pattern(&mv, vpat, scope, fail_lbl, bindings)?;
                    }
                }
                if let Some(rname) = rest {
                    let excl_items: Vec<String> =
                        excl_vars.iter().map(|v| format!("{v}.clone()")).collect();
                    let rv = self.fresh_tmp("pat_mr");
                    self.line(&format!(
                        "let {rv} = rt.pat_map_rest(&{subj_var}, &[{}])?;",
                        excl_items.join(", ")
                    ));
                    bindings.push((rname.clone(), rv));
                }
            }
            Pattern::Class { cls, pos, kw } => {
                let cv = self.emit_expr(cls, scope)?;
                let pv_list = self.fresh_tmp("pat_cpos");
                self.line(&format!(
                    "let Some({pv_list}) = rt.pat_class_pos(&{subj_var}, &{cv}, {})? else {{ break '{fail_lbl}; }};",
                    pos.len()
                ));
                for (idx, sub_pat) in pos.iter().enumerate() {
                    if !matches!(sub_pat, Pattern::Wildcard) {
                        let pv = self.fresh_tmp("pat_cp");
                        self.line(&format!("let {pv} = {pv_list}[{idx}].clone();"));
                        self.emit_aot_pattern(&pv, sub_pat, scope, fail_lbl, bindings)?;
                    }
                }
                for (kname, sub_pat) in kw {
                    let kv = self.fresh_tmp("pat_ckv");
                    self.line(&format!(
                        "let Some({kv}) = rt.pat_class_kw(&{subj_var}, {}) else {{ break '{fail_lbl}; }};",
                        escape_rust_str(kname)
                    ));
                    if !matches!(sub_pat, Pattern::Wildcard) {
                        self.emit_aot_pattern(&kv, sub_pat, scope, fail_lbl, bindings)?;
                    }
                }
            }
        }
        Ok(())
    }
}

pub(crate) fn compile_source_to_rust(src: &str) -> Result<String, String> {
    let tokens = lex_with_lines(src)?;
    let ast = Parser::new_with_lines(tokens).program()?;
    semantic_check_ast(&ast)?;
    validate_aot_support(&ast)?;

    let mut emitter = AotEmitter::new();
    emitter.out.push_str(AOT_PREAMBLE);
    emitter.out.push_str("\nmod bigint {\n");
    emitter.out.push_str(include_str!("bigint.rs"));
    emitter.out.push_str("\n}\n");
    emitter.line("fn vy_main(rt: &mut Rt) -> Result<(), Flow> {");
    emitter.indent = 1;
    let empty_parent = BTreeSet::new();
    let root_scope = emitter.build_scope(true, &[], &ast, &empty_parent, &empty_parent);
    emitter.emit_var_decls(&root_scope, &[]);
    emitter.emit_stmts(&ast, &root_scope)?;
    emitter.line("Ok(())");
    emitter.indent = 0;
    emitter.line("}");
    emitter.line("");
    emitter.line("fn main() {");
    emitter.line("    let mut rt = Rt::new();");
    emitter.line("    match vy_main(&mut rt) {");
    emitter.line("        Ok(()) | Err(Flow::Ret(_)) => {}");
    emitter.line("        Err(Flow::Err(e)) => {");
    emitter.line("            eprintln!(\"error: {e}\");");
    emitter.line("            std::process::exit(1);");
    emitter.line("        }");
    emitter.line("        Err(Flow::Break) | Err(Flow::Cont) => {");
    emitter.line("            eprintln!(\"error: loop control outside loop\");");
    emitter.line("            std::process::exit(1);");
    emitter.line("        }");
    emitter.line("    }");
    emitter.line("}");
    Ok(emitter.out)
}

pub(crate) fn compile_source_to_native(src: &str, out_exe: &Path) -> Result<(), String> {
    let rust_code = compile_source_to_rust(src)?;
    if let Some(parent) = out_exe.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
    }
    let id = AOT_TEMP_IDS.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let tmp_rs = env::temp_dir().join(format!("kovrek_aot_{pid}_{id}.rs"));
    fs::write(&tmp_rs, &rust_code).map_err(|e| e.to_string())?;
    let _guard = TempFileGuard(tmp_rs.clone());

    let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let output = Command::new(&rustc)
        .arg("--edition")
        .arg("2021")
        .arg("-C")
        .arg("opt-level=3")
        .arg("-C")
        .arg("debuginfo=0")
        .arg(&tmp_rs)
        .arg("-o")
        .arg(out_exe)
        .output()
        .map_err(|e| format!("failed to invoke native compiler '{rustc}': {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("native AOT compilation failed:\n{stderr}"));
    }
    Ok(())
}

pub(crate) fn compile_file_to_native(
    input: &Path,
    output: Option<&Path>,
) -> Result<PathBuf, String> {
    let src = load_source(input.to_string_lossy().as_ref())?;
    let out_path = match output {
        Some(p) => p.to_path_buf(),
        None => {
            if cfg!(target_os = "windows") {
                input.with_extension("exe")
            } else {
                let candidate = input.with_extension("");
                if candidate == input {
                    input.with_extension("out")
                } else {
                    candidate
                }
            }
        }
    };
    compile_source_to_native(&src, &out_path)?;
    println!("Built native executable {}", out_path.display());
    Ok(out_path)
}

const AOT_PREAMBLE: &str = r####"#![allow(
    dead_code,
    unused_variables,
    unused_mut,
    unreachable_code,
    unused_imports,
    unused_labels,
    clippy::all
)]

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::rc::Rc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, PartialEq)]
enum Val {
    Num(f64),
    Str(String),
    Bool(bool),
    Null,
    List(Vec<Val>),
    Map(Vec<(Val, Val)>),
    Object(usize),
    Fn(usize),
    Generator(usize),
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
            Val::Object(_) | Val::Fn(_) | Val::Generator(_) => true,
        }
    }

    fn show(&self) -> String {
        match self {
            Val::Num(n) => {
                if n.fract() == 0.0 && n.abs() < 9e15 {
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
            Val::Fn(id) => format!("<function:{id}>"),
            Val::Generator(id) => format!("<generator:{id}>"),
        }
    }
}

enum Flow {
    Ret(Val),
    Break,
    Cont,
    Err(String),
}

struct ObjData {
    class_name: String,
    fields: HashMap<String, Val>,
}

struct ClassInfo {
    bases: Vec<String>,
    metaclass: Option<String>,
    mro: Vec<String>,
    attrs: HashMap<String, Val>,
    methods: HashMap<String, usize>,
}

#[derive(Clone, Debug)]
enum VyJson {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<VyJson>),
    Obj(Vec<(String, VyJson)>),
}

impl VyJson {
    fn get(&self, key: &str) -> Option<&VyJson> {
        match self {
            VyJson::Obj(pairs) => pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    fn as_bool(&self) -> Option<bool> {
        match self {
            VyJson::Bool(b) => Some(*b),
            _ => None,
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            VyJson::Str(s) => Some(s.as_str()),
            _ => None,
        }
    }

    fn write_to(&self, out: &mut String) {
        match self {
            VyJson::Null => out.push_str("null"),
            VyJson::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            VyJson::Num(n) => {
                if !n.is_finite() {
                    out.push_str("null");
                } else if n.fract() == 0.0 && n.abs() < 9e15 {
                    out.push_str(&format!("{}", *n as i64));
                } else {
                    out.push_str(&format!("{n}"));
                }
            }
            VyJson::Str(s) => {
                out.push('"');
                for ch in s.chars() {
                    match ch {
                        '"' => out.push_str("\\\""),
                        '\\' => out.push_str("\\\\"),
                        '\n' => out.push_str("\\n"),
                        '\r' => out.push_str("\\r"),
                        '\t' => out.push_str("\\t"),
                        c if (c as u32) < 0x20 => {
                            out.push_str(&format!("\\u{:04x}", c as u32));
                        }
                        c => out.push(c),
                    }
                }
                out.push('"');
            }
            VyJson::Arr(xs) => {
                out.push('[');
                for (i, x) in xs.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    x.write_to(out);
                }
                out.push(']');
            }
            VyJson::Obj(pairs) => {
                out.push('{');
                for (i, (k, v)) in pairs.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    VyJson::Str(k.clone()).write_to(out);
                    out.push(':');
                    v.write_to(out);
                }
                out.push('}');
            }
        }
    }

    fn stringify(&self) -> String {
        let mut s = String::new();
        self.write_to(&mut s);
        s
    }

    fn parse(input: &str) -> Result<VyJson, String> {
        let mut idx = 0usize;
        let bytes = input.as_bytes();
        fn skip_ws(b: &[u8], i: &mut usize) {
            while *i < b.len() && matches!(b[*i], b' ' | b'\t' | b'\n' | b'\r') {
                *i += 1;
            }
        }
        fn parse_str(s: &str, b: &[u8], i: &mut usize) -> Result<String, String> {
            if *i >= b.len() || b[*i] != b'"' {
                return Err("expected string".into());
            }
            *i += 1;
            let mut out = String::new();
            while *i < b.len() {
                let byte = b[*i];
                if byte == b'"' {
                    *i += 1;
                    return Ok(out);
                }
                if byte == b'\\' {
                    *i += 1;
                    if *i >= b.len() {
                        return Err("unterminated escape".into());
                    }
                    match b[*i] {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\x08'),
                        b'f' => out.push('\x0c'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            if *i + 4 >= b.len() {
                                return Err("bad unicode escape".into());
                            }
                            let hex = &s[*i + 1..*i + 5];
                            let code = u16::from_str_radix(hex, 16)
                                .map_err(|_| "bad unicode hex".to_string())?;
                            *i += 4;
                            if (0xD800..=0xDBFF).contains(&code)
                                && *i + 6 < b.len()
                                && &b[*i + 1..*i + 3] == b"\\u"
                            {
                                let lo_hex = &s[*i + 3..*i + 7];
                                if let Ok(lo) = u16::from_str_radix(lo_hex, 16) {
                                    if (0xDC00..=0xDFFF).contains(&lo) {
                                        let cp = 0x10000
                                            + (((code as u32 - 0xD800) << 10)
                                                | (lo as u32 - 0xDC00));
                                        if let Some(ch) = char::from_u32(cp) {
                                            out.push(ch);
                                            *i += 6;
                                            *i += 1;
                                            continue;
                                        }
                                    }
                                }
                            }
                            if let Some(ch) = char::from_u32(code as u32) {
                                out.push(ch);
                            }
                        }
                        other => out.push(other as char),
                    }
                    *i += 1;
                } else {
                    let ch = s[*i..].chars().next().ok_or("utf8 error")?;
                    out.push(ch);
                    *i += ch.len_utf8();
                }
            }
            Err("unterminated string".into())
        }
        fn parse_val(s: &str, b: &[u8], i: &mut usize) -> Result<VyJson, String> {
            skip_ws(b, i);
            if *i >= b.len() {
                return Err("unexpected EOF".into());
            }
            match b[*i] {
                b'n' if s[*i..].starts_with("null") => {
                    *i += 4;
                    Ok(VyJson::Null)
                }
                b't' if s[*i..].starts_with("true") => {
                    *i += 4;
                    Ok(VyJson::Bool(true))
                }
                b'f' if s[*i..].starts_with("false") => {
                    *i += 5;
                    Ok(VyJson::Bool(false))
                }
                b'"' => Ok(VyJson::Str(parse_str(s, b, i)?)),
                b'[' => {
                    *i += 1;
                    let mut arr = Vec::new();
                    skip_ws(b, i);
                    if *i < b.len() && b[*i] == b']' {
                        *i += 1;
                        return Ok(VyJson::Arr(arr));
                    }
                    loop {
                        arr.push(parse_val(s, b, i)?);
                        skip_ws(b, i);
                        if *i < b.len() && b[*i] == b',' {
                            *i += 1;
                            continue;
                        }
                        if *i < b.len() && b[*i] == b']' {
                            *i += 1;
                            return Ok(VyJson::Arr(arr));
                        }
                        return Err("expected ',' or ']'".into());
                    }
                }
                b'{' => {
                    *i += 1;
                    let mut obj = Vec::new();
                    skip_ws(b, i);
                    if *i < b.len() && b[*i] == b'}' {
                        *i += 1;
                        return Ok(VyJson::Obj(obj));
                    }
                    loop {
                        skip_ws(b, i);
                        let key = parse_str(s, b, i)?;
                        skip_ws(b, i);
                        if *i >= b.len() || b[*i] != b':' {
                            return Err("expected ':'".into());
                        }
                        *i += 1;
                        let val = parse_val(s, b, i)?;
                        obj.push((key, val));
                        skip_ws(b, i);
                        if *i < b.len() && b[*i] == b',' {
                            *i += 1;
                            continue;
                        }
                        if *i < b.len() && b[*i] == b'}' {
                            *i += 1;
                            return Ok(VyJson::Obj(obj));
                        }
                        return Err("expected ',' or '}'".into());
                    }
                }
                b'-' | b'0'..=b'9' => {
                    let start = *i;
                    if b[*i] == b'-' {
                        *i += 1;
                    }
                    while *i < b.len() && matches!(b[*i], b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-') {
                        if (*i > start) && matches!(b[*i], b'+' | b'-') && !matches!(b[*i - 1], b'e' | b'E') {
                            break;
                        }
                        *i += 1;
                    }
                    let num = s[start..*i]
                        .parse::<f64>()
                        .map_err(|_| "invalid number".to_string())?;
                    Ok(VyJson::Num(num))
                }
                _ => Err("unexpected token in json".into()),
            }
        }
        parse_val(input, bytes, &mut idx)
    }
}

fn aot_val_to_big_arg(v: &Val) -> bigint::BigArg {
    match v {
        Val::Str(s) => bigint::BigArg::Str(s.clone()),
        Val::Num(n) => bigint::BigArg::Num(*n),
        Val::List(xs) => bigint::BigArg::List(xs.iter().map(aot_val_to_big_arg).collect()),
        Val::Map(pairs) => {
            let mut re = None;
            let mut im = 0.0;
            for (k, val) in pairs {
                if let (Val::Str(ks), Val::Num(x)) = (k, val) {
                    if ks == "re" {
                        re = Some(*x);
                    } else if ks == "im" {
                        im = *x;
                    }
                }
            }
            match re {
                Some(r) => bigint::BigArg::Complex(r, im),
                None => bigint::BigArg::Other,
            }
        }
        _ => bigint::BigArg::Other,
    }
}

fn aot_big_out_to_val(out: bigint::BigOut) -> Val {
    match out {
        bigint::BigOut::Str(s) => Val::Str(s),
        bigint::BigOut::Num(n) => Val::Num(n),
        bigint::BigOut::StrPair(q, r) => Val::List(vec![Val::Str(q), Val::Str(r)]),
        bigint::BigOut::Complex(r, i) => Val::List(vec![Val::Num(r), Val::Num(i)]),
        bigint::BigOut::ComplexList(zs) => Val::List(
            zs.into_iter()
                .map(|(r, i)| Val::List(vec![Val::Num(r), Val::Num(i)]))
                .collect(),
        ),
    }
}

fn is_py_handle_id(val: &Val) -> Option<u64> {
    let Val::Map(pairs) = val else {
        return None;
    };
    let mut id = None;
    let mut has_type = false;
    for (k, v) in pairs {
        match (k, v) {
            (Val::Str(ks), Val::Num(n)) if ks == "__py_handle__" && n.is_finite() => {
                id = Some(*n as u64);
            }
            (Val::Str(ks), Val::Str(_)) if ks == "__py_type__" => {
                has_type = true;
            }
            _ => {}
        }
    }
    if has_type && pairs.len() == 2 {
        id
    } else {
        None
    }
}

fn val_to_wire(val: &Val) -> VyJson {
    if let Some(id) = is_py_handle_id(val) {
        return VyJson::Obj(vec![("__py_ref__".into(), VyJson::Num(id as f64))]);
    }
    match val {
        Val::Null => VyJson::Null,
        Val::Bool(b) => VyJson::Bool(*b),
        Val::Num(n) => VyJson::Num(*n),
        Val::Str(s) => VyJson::Str(s.clone()),
        Val::List(xs) => VyJson::Arr(xs.iter().map(val_to_wire).collect()),
        Val::Map(pairs) => VyJson::Obj(
            pairs
                .iter()
                .map(|(k, v)| (k.show(), val_to_wire(v)))
                .collect(),
        ),
        other => VyJson::Str(other.show()),
    }
}

fn wire_to_val(j: VyJson) -> Val {
    match j {
        VyJson::Null => Val::Null,
        VyJson::Bool(b) => Val::Bool(b),
        VyJson::Num(n) => Val::Num(n),
        VyJson::Str(s) => Val::Str(s),
        VyJson::Arr(xs) => Val::List(xs.into_iter().map(wire_to_val).collect()),
        VyJson::Obj(pairs) => Val::Map(
            pairs
                .into_iter()
                .map(|(k, v)| (Val::Str(k), wire_to_val(v)))
                .collect(),
        ),
    }
}

const PY_WORKER_SCRIPT: &str = r#"
import importlib, json, sys

_PROTO_OUT = sys.stdout
sys.stdout = sys.stderr

_OBJS = {}
_NEXT_ID = 1
_ENV = {"__builtins__": __builtins__}

def _alloc(obj):
    global _NEXT_ID
    oid = _NEXT_ID
    _NEXT_ID += 1
    _OBJS[oid] = obj
    return {"__py_handle__": oid, "__py_type__": type(obj).__name__}

def _to_wire(v):
    if v is None or isinstance(v, (bool, int, float, str)):
        return v
    if isinstance(v, (list, tuple)):
        return [_to_wire(x) for x in v]
    if type(v) is dict:
        out = {}
        for k, val in v.items():
            out[str(k)] = _to_wire(val)
        if "__py_handle__" in out and len(out) == 2 and "__py_type__" in out:
            return _alloc(v)
        return out
    return _alloc(v)

def _from_wire(x):
    if isinstance(x, list):
        return [_from_wire(v) for v in x]
    if isinstance(x, dict):
        if "__py_ref__" in x and len(x) == 1:
            oid = int(x["__py_ref__"])
            if oid not in _OBJS:
                raise KeyError(f"dangling python handle {oid}")
            return _OBJS[oid]
        return {k: _from_wire(v) for k, v in x.items()}
    return x

def _dispatch(req):
    op = req["op"]
    if op == "import":
        return _to_wire(importlib.import_module(req["name"]))
    if op == "call":
        fn = _from_wire(req["fn"])
        args = _from_wire(req.get("args", []))
        kwargs = _from_wire(req.get("kwargs", {})) or {}
        return _to_wire(fn(*args, **kwargs)) if isinstance(kwargs, dict) else _to_wire(fn(*args))
    if op == "call_method":
        obj = _from_wire(req["obj"])
        method = req["method"]
        args = _from_wire(req.get("args", []))
        kwargs = _from_wire(req.get("kwargs", {})) or {}
        return _to_wire(getattr(obj, method)(*args, **kwargs))
    if op == "get":
        return _to_wire(getattr(_from_wire(req["obj"]), req["attr"]))
    if op == "set":
        obj = _from_wire(req["obj"])
        val = _from_wire(req["val"])
        setattr(obj, req["attr"], val)
        return _to_wire(val)
    if op == "hasattr":
        return bool(hasattr(_from_wire(req["obj"]), req["attr"]))
    if op == "getitem":
        return _to_wire(_from_wire(req["obj"])[_from_wire(req["key"])])
    if op == "setitem":
        obj = _from_wire(req["obj"])
        val = _from_wire(req["val"])
        obj[_from_wire(req["key"])] = val
        return _to_wire(val)
    if op == "dir":
        return [str(x) for x in dir(_from_wire(req["obj"]))]
    if op == "str":
        return str(_from_wire(req["obj"]))
    if op == "repr":
        return repr(_from_wire(req["obj"]))
    if op == "eval":
        loc = _from_wire(req.get("locals", {})) or {}
        if not isinstance(loc, dict):
            raise TypeError("py_eval locals must be a dict/Map")
        return _to_wire(eval(req["expr"], _ENV, loc))
    if op == "exec":
        exec(req["code"], _ENV)
        return None
    if op == "free":
        obj = req.get("obj") or {}
        oid = int(obj["__py_ref__"]) if isinstance(obj, dict) and "__py_ref__" in obj else None
        return _OBJS.pop(oid, None) is not None if oid is not None else False
    raise ValueError(f"unknown op: {op}")

for raw in sys.stdin:
    raw = raw.strip()
    if not raw:
        continue
    try:
        req = json.loads(raw)
        resp = {"ok": True, "val": _dispatch(req)}
    except Exception as ex:
        resp = {"ok": False, "err": f"{type(ex).__name__}: {ex}"}
    _PROTO_OUT.write(json.dumps(resp) + "\n")
    _PROTO_OUT.flush()
"#;

struct PyWorker {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl PyWorker {
    fn detect() -> Option<(&'static str, &'static [&'static str])> {
        let candidates: [(&'static str, &'static [&'static str]); 3] =
            [("python", &[]), ("python3", &[]), ("py", &["-3"])];
        for (exe, prefix) in candidates {
            if let Ok(out) = Command::new(exe).args(prefix).arg("--version").output() {
                if out.status.success() {
                    return Some((exe, prefix));
                }
            }
        }
        None
    }

    fn spawn() -> Result<Self, String> {
        let (exe, prefix) =
            Self::detect().ok_or("no Python 3 interpreter found on PATH (tried python, python3, py)")?;
        let mut child = Command::new(exe)
            .args(prefix)
            .arg("-u")
            .arg("-c")
            .arg(PY_WORKER_SCRIPT)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("failed to launch Python worker ({exe}): {e}"))?;
        let stdin = child.stdin.take().ok_or("missing Python stdin pipe")?;
        let stdout = BufReader::new(child.stdout.take().ok_or("missing Python stdout pipe")?);
        Ok(Self {
            child,
            stdin,
            stdout,
        })
    }

    fn request(&mut self, payload: &VyJson) -> Result<VyJson, String> {
        let line = payload.stringify();
        self.stdin
            .write_all(line.as_bytes())
            .and_then(|_| self.stdin.write_all(b"\n"))
            .and_then(|_| self.stdin.flush())
            .map_err(|e| format!("python worker write failed: {e}"))?;
        let mut resp_line = String::new();
        let n = self
            .stdout
            .read_line(&mut resp_line)
            .map_err(|e| format!("python worker read failed: {e}"))?;
        if n == 0 {
            return Err("python worker exited unexpectedly".into());
        }
        let resp = VyJson::parse(resp_line.trim())?;
        if resp.get("ok").and_then(VyJson::as_bool) == Some(true) {
            Ok(resp.get("val").cloned().unwrap_or(VyJson::Null))
        } else {
            let msg = resp
                .get("err")
                .and_then(VyJson::as_str)
                .unwrap_or("python error");
            Err(format!("python: {msg}"))
        }
    }
}

type NativeFn = Rc<dyn Fn(&mut Rt, Vec<Val>, Vec<(String, Val)>) -> Result<Val, Flow>>;

struct Rt {
    fns: Vec<NativeFn>,
    named_fns: HashMap<String, usize>,
    classes: HashMap<String, ClassInfo>,
    class_objects: HashMap<String, usize>,
    builtin_fn_ids: HashMap<String, usize>,
    fn_params: HashMap<usize, Vec<String>>,
    call_ctx_stack: Vec<(Option<String>, Option<Val>)>,
    objects: Vec<ObjData>,
    generators: Vec<(Vec<Val>, usize)>,
    tasks: HashMap<u64, Option<Result<Val, String>>>,
    next_task_id: u64,
    channels: HashMap<u64, VecDeque<Val>>,
    next_channel_id: u64,
    globals: HashMap<String, Val>,
    start: Instant,
    py_worker: Option<PyWorker>,
}

impl Drop for Rt {
    fn drop(&mut self) {
        if let Some(mut w) = self.py_worker.take() {
            let _ = w.child.kill();
            let _ = w.child.wait();
        }
    }
}

impl Rt {
    fn builtin_classes_map() -> HashMap<String, ClassInfo> {
        let mut classes = HashMap::new();
        let make = |bases: Vec<String>, mro: Vec<String>| ClassInfo {
            bases,
            metaclass: None,
            mro,
            attrs: HashMap::new(),
            methods: HashMap::new(),
        };
        classes.insert("object".into(), make(vec![], vec!["object".into()]));
        for name in [
            "type", "int", "float", "str", "bool", "list", "dict", "function", "null",
        ] {
            classes.insert(
                name.into(),
                make(vec!["object".into()], vec![name.into(), "object".into()]),
            );
        }
        classes
    }

    fn new() -> Self {
        Self {
            fns: Vec::new(),
            named_fns: HashMap::new(),
            classes: Self::builtin_classes_map(),
            class_objects: HashMap::new(),
            builtin_fn_ids: HashMap::new(),
            fn_params: HashMap::new(),
            call_ctx_stack: Vec::new(),
            objects: Vec::new(),
            generators: Vec::new(),
            tasks: HashMap::new(),
            next_task_id: 1,
            channels: HashMap::new(),
            next_channel_id: 1,
            globals: HashMap::new(),
            start: Instant::now(),
            py_worker: None,
        }
    }

    fn py_req(&mut self, payload: VyJson) -> Result<VyJson, Flow> {
        if self.py_worker.is_none() {
            self.py_worker = Some(PyWorker::spawn().map_err(Flow::Err)?);
        }
        let res = self.py_worker.as_mut().unwrap().request(&payload);
        match res {
            Ok(v) => Ok(v),
            Err(err)
                if err.starts_with("python worker write failed")
                    || err.starts_with("python worker read failed")
                    || err == "python worker exited unexpectedly" =>
            {
                if let Some(mut old) = self.py_worker.take() {
                    let _ = old.child.kill();
                    let _ = old.child.wait();
                }
                let mut fresh = PyWorker::spawn().map_err(Flow::Err)?;
                let retry = fresh.request(&payload).map_err(Flow::Err);
                self.py_worker = Some(fresh);
                retry
            }
            Err(e) => Err(Flow::Err(e)),
        }
    }

    fn py_import(&mut self, name: &str) -> Result<Val, Flow> {
        let v = self.py_req(VyJson::Obj(vec![
            ("op".into(), VyJson::Str("import".into())),
            ("name".into(), VyJson::Str(name.into())),
        ]))?;
        Ok(wire_to_val(v))
    }

    fn py_kwargs_json(kwargs: &[(String, Val)]) -> VyJson {
        VyJson::Obj(
            kwargs
                .iter()
                .map(|(k, v)| (k.clone(), val_to_wire(v)))
                .collect(),
        )
    }

    fn py_call(&mut self, fn_val: &Val, args: &[Val], kwargs: &[(String, Val)]) -> Result<Val, Flow> {
        let v = self.py_req(VyJson::Obj(vec![
            ("op".into(), VyJson::Str("call".into())),
            ("fn".into(), val_to_wire(fn_val)),
            ("args".into(), VyJson::Arr(args.iter().map(val_to_wire).collect())),
            ("kwargs".into(), Self::py_kwargs_json(kwargs)),
        ]))?;
        Ok(wire_to_val(v))
    }

    fn py_call_method(
        &mut self,
        obj: &Val,
        method: &str,
        args: &[Val],
        kwargs: &[(String, Val)],
    ) -> Result<Val, Flow> {
        let v = self.py_req(VyJson::Obj(vec![
            ("op".into(), VyJson::Str("call_method".into())),
            ("obj".into(), val_to_wire(obj)),
            ("method".into(), VyJson::Str(method.into())),
            ("args".into(), VyJson::Arr(args.iter().map(val_to_wire).collect())),
            ("kwargs".into(), Self::py_kwargs_json(kwargs)),
        ]))?;
        Ok(wire_to_val(v))
    }

    fn py_get(&mut self, obj: &Val, attr: &str) -> Result<Val, Flow> {
        let v = self.py_req(VyJson::Obj(vec![
            ("op".into(), VyJson::Str("get".into())),
            ("obj".into(), val_to_wire(obj)),
            ("attr".into(), VyJson::Str(attr.into())),
        ]))?;
        Ok(wire_to_val(v))
    }

    fn py_set(&mut self, obj: &Val, attr: &str, val: &Val) -> Result<Val, Flow> {
        let v = self.py_req(VyJson::Obj(vec![
            ("op".into(), VyJson::Str("set".into())),
            ("obj".into(), val_to_wire(obj)),
            ("attr".into(), VyJson::Str(attr.into())),
            ("val".into(), val_to_wire(val)),
        ]))?;
        Ok(wire_to_val(v))
    }

    fn py_hasattr(&mut self, obj: &Val, attr: &str) -> Result<bool, Flow> {
        let v = self.py_req(VyJson::Obj(vec![
            ("op".into(), VyJson::Str("hasattr".into())),
            ("obj".into(), val_to_wire(obj)),
            ("attr".into(), VyJson::Str(attr.into())),
        ]))?;
        Ok(v.as_bool().unwrap_or(false))
    }

    fn py_getitem(&mut self, obj: &Val, key: &Val) -> Result<Val, Flow> {
        let v = self.py_req(VyJson::Obj(vec![
            ("op".into(), VyJson::Str("getitem".into())),
            ("obj".into(), val_to_wire(obj)),
            ("key".into(), val_to_wire(key)),
        ]))?;
        Ok(wire_to_val(v))
    }

    fn py_setitem(&mut self, obj: &Val, key: &Val, val: &Val) -> Result<Val, Flow> {
        let v = self.py_req(VyJson::Obj(vec![
            ("op".into(), VyJson::Str("setitem".into())),
            ("obj".into(), val_to_wire(obj)),
            ("key".into(), val_to_wire(key)),
            ("val".into(), val_to_wire(val)),
        ]))?;
        Ok(wire_to_val(v))
    }

    fn py_dir(&mut self, obj: &Val) -> Result<Val, Flow> {
        let v = self.py_req(VyJson::Obj(vec![
            ("op".into(), VyJson::Str("dir".into())),
            ("obj".into(), val_to_wire(obj)),
        ]))?;
        Ok(wire_to_val(v))
    }

    fn py_str(&mut self, obj: &Val) -> Result<String, Flow> {
        let v = self.py_req(VyJson::Obj(vec![
            ("op".into(), VyJson::Str("str".into())),
            ("obj".into(), val_to_wire(obj)),
        ]))?;
        Ok(v.as_str().unwrap_or("").to_string())
    }

    fn py_repr(&mut self, obj: &Val) -> Result<String, Flow> {
        let v = self.py_req(VyJson::Obj(vec![
            ("op".into(), VyJson::Str("repr".into())),
            ("obj".into(), val_to_wire(obj)),
        ]))?;
        Ok(v.as_str().unwrap_or("").to_string())
    }

    fn py_eval(&mut self, expr: &str, locals: Option<&Val>) -> Result<Val, Flow> {
        let loc_json = match locals {
            Some(v) => val_to_wire(v),
            None => VyJson::Obj(vec![]),
        };
        let v = self.py_req(VyJson::Obj(vec![
            ("op".into(), VyJson::Str("eval".into())),
            ("expr".into(), VyJson::Str(expr.into())),
            ("locals".into(), loc_json),
        ]))?;
        Ok(wire_to_val(v))
    }

    fn py_exec(&mut self, code: &str) -> Result<(), Flow> {
        let _ = self.py_req(VyJson::Obj(vec![
            ("op".into(), VyJson::Str("exec".into())),
            ("code".into(), VyJson::Str(code.into())),
        ]))?;
        Ok(())
    }

    fn py_free(&mut self, obj: &Val) -> Result<bool, Flow> {
        if is_py_handle_id(obj).is_none() {
            return Ok(false);
        }
        let v = self.py_req(VyJson::Obj(vec![
            ("op".into(), VyJson::Str("free".into())),
            ("obj".into(), val_to_wire(obj)),
        ]))?;
        Ok(v.as_bool().unwrap_or(false))
    }

    fn reg_fn(&mut self, f: NativeFn) -> Val {
        let id = self.fns.len();
        self.fns.push(f);
        Val::Fn(id)
    }

    fn reg_fn_params(&mut self, val: &Val, params: &[&str]) {
        if let Val::Fn(id) = *val {
            self.fn_params
                .insert(id, params.iter().map(|s| s.to_string()).collect());
        }
    }

    fn pat_seq_split(
        &self,
        val: &Val,
        before: usize,
        has_star: bool,
        after: usize,
    ) -> Option<Vec<Val>> {
        let Val::List(items) = val else {
            return None;
        };
        let len = items.len();
        if has_star {
            if len < before + after {
                return None;
            }
            let mut out = Vec::with_capacity(before + 1 + after);
            out.extend_from_slice(&items[..before]);
            out.push(Val::List(items[before..(len - after)].to_vec()));
            out.extend_from_slice(&items[(len - after)..]);
            Some(out)
        } else {
            if len != before {
                return None;
            }
            Some(items.clone())
        }
    }

    fn pat_map_get(&self, val: &Val, key: &Val) -> Option<Val> {
        let Val::Map(pairs) = val else {
            return None;
        };
        pairs.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }

    fn pat_map_rest(&self, val: &Val, excluded: &[Val]) -> Result<Val, Flow> {
        let Val::Map(pairs) = val else {
            return Err(Flow::Err("mapping rest pattern requires Map".into()));
        };
        Ok(Val::Map(
            pairs
                .iter()
                .filter(|(k, _)| !excluded.iter().any(|ek| ek == k))
                .cloned()
                .collect(),
        ))
    }

    fn pat_class_pos(
        &mut self,
        val: &Val,
        cls: &Val,
        pos_len: usize,
    ) -> Result<Option<Vec<Val>>, Flow> {
        let target = self.resolve_class_name_arg(cls)?;
        if !self.value_matches_rt(val, &target) {
            return Ok(None);
        }
        if pos_len == 0 {
            return Ok(Some(Vec::new()));
        }
        if matches!(
            target.as_str(),
            "int" | "float" | "str" | "bool" | "list" | "dict" | "function" | "object"
        ) {
            if pos_len == 1 {
                return Ok(Some(vec![val.clone()]));
            }
            return Err(Flow::Err(format!(
                "{target}() accepts at most 1 positional sub-pattern ({pos_len} given)"
            )));
        }
        let mro = self.class_mro_slice(&target);
        let mut attr_names: Option<Vec<String>> = None;
        for cname in &mro {
            if let Some(cinfo) = self.classes.get(cname) {
                if let Some(Val::List(items)) = cinfo.attrs.get("__match_args__") {
                    let mut names = Vec::with_capacity(items.len());
                    for item in items {
                        let Val::Str(s) = item else {
                            return Err(Flow::Err(format!(
                                "{target}.__match_args__ must contain strings"
                            )));
                        };
                        names.push(s.clone());
                    }
                    attr_names = Some(names);
                    break;
                }
            }
        }
        let attr_names = if let Some(names) = attr_names {
            names
        } else {
            let mut found = Vec::new();
            for cname in &mro {
                if let Some(cinfo) = self.classes.get(cname) {
                    if let Some(&fid) = cinfo
                        .methods
                        .get("init")
                        .or_else(|| cinfo.methods.get("__init__"))
                    {
                        if let Some(pnames) = self.fn_params.get(&fid) {
                            for p in pnames {
                                if p != "self" {
                                    found.push(p.clone());
                                }
                            }
                        }
                        break;
                    }
                }
            }
            found
        };
        if pos_len > attr_names.len() {
            return Err(Flow::Err(format!(
                "{target}() accepts {} positional sub-patterns ({pos_len} given)",
                attr_names.len()
            )));
        }
        let mut out = Vec::with_capacity(pos_len);
        for name in &attr_names[..pos_len] {
            match self.get_member(val.clone(), name) {
                Ok(v) => out.push(v),
                Err(_) => return Ok(None),
            }
        }
        Ok(Some(out))
    }

    fn pat_class_kw(&mut self, val: &Val, attr: &str) -> Option<Val> {
        self.get_member(val.clone(), attr).ok()
    }

    fn new_generator(&mut self, items: Vec<Val>) -> Val {
        let id = self.generators.len();
        self.generators.push((items, 0));
        Val::Generator(id)
    }

    fn store_task(&mut self, res: Result<Val, String>) -> Val {
        let id = self.next_task_id;
        self.next_task_id += 1;
        self.tasks.insert(id, Some(res));
        Val::Num(id as f64)
    }

    fn await_val(&mut self, val: Val) -> Result<Val, Flow> {
        let Val::Num(n) = val else {
            return Err(Flow::Err("await expects task id".into()));
        };
        let id = n as u64;
        match self.tasks.remove(&id).flatten() {
            Some(Ok(v)) => Ok(v),
            Some(Err(e)) => Err(Flow::Err(e)),
            None => Err(Flow::Err(format!("unknown task {id}"))),
        }
    }

    fn set_global(&mut self, name: &str, v: Val) {
        self.globals.insert(name.to_string(), v);
    }

    fn resolve_ident(&mut self, name: &str) -> Result<Val, Flow> {
        if let Some(v) = self.globals.get(name) {
            return Ok(v.clone());
        }
        if let Some(&id) = self.named_fns.get(name) {
            return Ok(Val::Fn(id));
        }
        if self.classes.contains_key(name) {
            return self.class_object_val(name);
        }
        if matches!(
            name,
            "property"
                | "staticmethod"
                | "classmethod"
                | "super"
                | "isinstance"
                | "is_instance"
                | "issubclass"
                | "mro"
                | "getattr"
                | "hasattr"
                | "setattr"
                | "delattr"
                | "type_of"
                | "callable"
        ) {
            return Ok(self.builtin_fn_val(name));
        }
        Err(Flow::Err(format!("undefined name '{name}'")))
    }

    fn builtin_fn_val(&mut self, name: &str) -> Val {
        if let Some(&id) = self.builtin_fn_ids.get(name) {
            return Val::Fn(id);
        }
        let bname = name.to_string();
        let val = self.reg_fn(Rc::new(
            move |rt: &mut Rt, args: Vec<Val>, kwargs: Vec<(String, Val)>| {
                rt.call_builtin_wrapper(&bname, args, kwargs)
            },
        ));
        if let Val::Fn(id) = val {
            self.builtin_fn_ids.insert(name.to_string(), id);
        }
        val
    }

    fn pop_kw(kw: &mut Vec<(String, Val)>, key: &str) -> Option<Val> {
        kw.iter()
            .position(|(k, _)| k == key)
            .map(|idx| kw.remove(idx).1)
    }

    fn compute_c3_mro(
        &self,
        cls_name: &str,
        bases: &[String],
    ) -> Result<Vec<String>, Flow> {
        if bases.is_empty() {
            return Ok(vec![cls_name.to_string()]);
        }
        let mut seqs: Vec<Vec<String>> = Vec::with_capacity(bases.len() + 1);
        for b in bases {
            let cls = self
                .classes
                .get(b)
                .ok_or_else(|| Flow::Err(format!("unknown base class '{b}'")))?;
            let m = if cls.mro.is_empty() {
                vec![b.clone(), "object".to_string()]
            } else {
                cls.mro.clone()
            };
            seqs.push(m);
        }
        seqs.push(bases.to_vec());
        let mut out = vec![cls_name.to_string()];
        loop {
            seqs.retain(|s| !s.is_empty());
            if seqs.is_empty() {
                break;
            }
            let mut chosen: Option<String> = None;
            for seq in &seqs {
                let cand = &seq[0];
                let in_tail = seqs.iter().any(|other| other[1..].iter().any(|x| x == cand));
                if !in_tail {
                    chosen = Some(cand.clone());
                    break;
                }
            }
            let Some(head) = chosen else {
                return Err(Flow::Err(format!(
                    "Cannot create a consistent method resolution order (MRO) for bases of {cls_name}"
                )));
            };
            if !out.contains(&head) {
                out.push(head.clone());
            }
            for seq in &mut seqs {
                if seq.first().is_some_and(|x| x == &head) {
                    seq.remove(0);
                }
            }
        }
        Ok(out)
    }

    fn class_mro_slice(&self, class_name: &str) -> Vec<String> {
        if let Some(cls) = self.classes.get(class_name) {
            if !cls.mro.is_empty() {
                return cls.mro.clone();
            }
        }
        vec![class_name.to_string(), "object".to_string()]
    }

    fn alloc_object(&mut self, class_name: &str, fields: HashMap<String, Val>) -> Val {
        let id = self.objects.len();
        self.objects.push(ObjData {
            class_name: class_name.to_string(),
            fields,
        });
        Val::Object(id)
    }

    fn class_object_tag(&self, val: &Val) -> Option<String> {
        let Val::Object(id) = val else {
            return None;
        };
        self.objects.get(*id).and_then(|o| match o.fields.get("__kovrek_class__") {
            Some(Val::Str(s)) => Some(s.clone()),
            _ => None,
        })
    }

    fn resolve_class_name_arg(&self, val: &Val) -> Result<String, Flow> {
        if let Some(cls) = self.class_object_tag(val) {
            return Ok(cls);
        }
        if let Val::Str(name) = val {
            if self.classes.contains_key(name) {
                return Ok(name.clone());
            }
        }
        Err(Flow::Err(format!("expected class/type, got {}", val.show())))
    }

    fn class_object_val(&mut self, cls_name: &str) -> Result<Val, Flow> {
        if !self.classes.contains_key(cls_name) {
            return Err(Flow::Err(format!("unknown class '{cls_name}'")));
        }
        if let Some(&id) = self.class_objects.get(cls_name) {
            if id < self.objects.len() {
                return Ok(Val::Object(id));
            }
        }
        let meta_name = self
            .classes
            .get(cls_name)
            .and_then(|c| c.metaclass.clone())
            .unwrap_or_else(|| "type".to_string());
        let mut fields = HashMap::new();
        fields.insert("__kovrek_class__".to_string(), Val::Str(cls_name.to_string()));
        fields.insert("__name__".to_string(), Val::Str(cls_name.to_string()));
        let obj = self.alloc_object(&meta_name, fields);
        if let Val::Object(id) = obj {
            self.class_objects.insert(cls_name.to_string(), id);
        }
        Ok(obj)
    }

    fn make_bound_method(&mut self, func: Val, self_val: Val, owner: Option<&str>) -> Val {
        let mut fields = HashMap::new();
        fields.insert("__func__".to_string(), func);
        fields.insert("__self__".to_string(), self_val);
        if let Some(o) = owner {
            fields.insert("__owner__".to_string(), Val::Str(o.to_string()));
        }
        self.alloc_object("__bound_method__", fields)
    }

    fn make_property_obj(&mut self, fget: Val, fset: Val, fdel: Val) -> Val {
        let mut fields = HashMap::new();
        fields.insert("fget".to_string(), fget);
        fields.insert("fset".to_string(), fset);
        fields.insert("fdel".to_string(), fdel);
        self.alloc_object("__property__", fields)
    }

    fn make_descriptor_wrapper(&mut self, tag: &str, func: Val) -> Val {
        let mut fields = HashMap::new();
        fields.insert("__func__".to_string(), func);
        self.alloc_object(tag, fields)
    }

    fn make_super_obj(&mut self, args: &[Val]) -> Result<Val, Flow> {
        let (thisclass, self_val) = if args.is_empty() {
            let Some((cls_opt, self_opt)) = self.call_ctx_stack.last().cloned() else {
                return Err(Flow::Err("super(): no active class context".into()));
            };
            let cls = cls_opt.ok_or_else(|| Flow::Err("super(): no active class context".into()))?;
            let sv = self_opt.ok_or_else(|| Flow::Err("super(): no active self/cls context".into()))?;
            (cls, sv)
        } else if args.len() == 2 {
            let cls = self.resolve_class_name_arg(&args[0])?;
            (cls, args[1].clone())
        } else {
            return Err(Flow::Err("super() expects 0 or 2 arguments".into()));
        };
        let mut fields = HashMap::new();
        fields.insert("__thisclass__".to_string(), Val::Str(thisclass));
        fields.insert("__self__".to_string(), self_val);
        Ok(self.alloc_object("__super__", fields))
    }

    fn define_class(
        &mut self,
        name: String,
        parent: Option<String>,
        methods: HashMap<String, usize>,
    ) -> Result<(), Flow> {
        if let Some(ref p) = parent {
            if !self.classes.contains_key(p) {
                return Err(Flow::Err(format!("unknown parent class '{p}'")));
            }
        }
        let bases = match parent {
            Some(p) => vec![p],
            None => vec!["object".to_string()],
        };
        let mro = self.compute_c3_mro(&name, &bases)?;
        self.classes.insert(
            name.clone(),
            ClassInfo {
                bases,
                metaclass: None,
                mro,
                attrs: HashMap::new(),
                methods,
            },
        );
        self.class_objects.remove(&name);
        let cls_obj = self.class_object_val(&name)?;
        self.globals.insert(name, cls_obj);
        Ok(())
    }

    fn register_class_from_ns(
        &mut self,
        meta_name: &str,
        cls_name: &str,
        bases_in: &[String],
        ns_pairs: Vec<(Val, Val)>,
    ) -> Result<Val, Flow> {
        let bases = if bases_in.is_empty() && cls_name != "object" {
            vec!["object".to_string()]
        } else {
            bases_in.to_vec()
        };
        let mro = self.compute_c3_mro(cls_name, &bases)?;
        let mut attrs = HashMap::new();
        let mut methods = HashMap::new();
        let builtin_ids: std::collections::HashSet<usize> =
            self.builtin_fn_ids.values().copied().collect();
        for (k, v) in ns_pairs {
            let key = k.show();
            if let Val::Fn(fid) = v {
                if !builtin_ids.contains(&fid) {
                    methods.insert(key, fid);
                    continue;
                }
            }
            attrs.insert(key, v);
        }
        self.classes.insert(
            cls_name.to_string(),
            ClassInfo {
                bases,
                metaclass: if meta_name == "type" {
                    None
                } else {
                    Some(meta_name.to_string())
                },
                mro,
                attrs,
                methods,
            },
        );
        self.class_objects.remove(cls_name);
        self.class_object_val(cls_name)
    }

    fn build_class(
        &mut self,
        name: String,
        raw_bases: Vec<Val>,
        meta_val: Val,
        body_fn: Val,
    ) -> Result<Val, Flow> {
        let mut bases = Vec::with_capacity(raw_bases.len());
        for b in &raw_bases {
            bases.push(self.resolve_class_name_arg(b)?);
        }
        if bases.is_empty() && name != "object" {
            bases.push("object".to_string());
        }
        let ns_val = self.call_val(&body_fn, vec![])?;
        let Val::Map(ns_pairs) = ns_val else {
            return Err(Flow::Err("internal error: class body did not return namespace map".into()));
        };

        let meta_name = if !matches!(meta_val, Val::Null) {
            if let Ok(mname) = self.resolve_class_name_arg(&meta_val) {
                mname
            } else {
                let mut base_objs = Vec::with_capacity(bases.len());
                for b in &bases {
                    base_objs.push(self.class_object_val(b)?);
                }
                return self.call_val(
                    &meta_val,
                    vec![Val::Str(name), Val::List(base_objs), Val::Map(ns_pairs)],
                );
            }
        } else {
            let mut chosen = "type".to_string();
            for b in &bases {
                if let Some(bm) = self.classes.get(b).and_then(|c| c.metaclass.clone()) {
                    if self.class_is_a(&bm, &chosen) {
                        chosen = bm;
                    } else if !self.class_is_a(&chosen, &bm) {
                        return Err(Flow::Err("metaclass conflict among class bases".into()));
                    }
                }
            }
            chosen
        };

        if meta_name == "type" {
            return self.register_class_from_ns("type", &name, &bases, ns_pairs);
        }

        let meta_cls_obj = self.class_object_val(&meta_name)?;
        let mut base_objs = Vec::with_capacity(bases.len());
        for b in &bases {
            base_objs.push(self.class_object_val(b)?);
        }
        let bases_list = Val::List(base_objs);
        let ns_map_val = Val::Map(ns_pairs);

        let custom_new = self
            .find_method_with_owner(&meta_name, "__new__")
            .filter(|(o, _)| o != "type" && o != "object");
        let cls_obj = if let Some((new_owner, new_id)) = custom_new {
            self.call_fn_id_ctx(
                new_id,
                vec![
                    meta_cls_obj.clone(),
                    Val::Str(name.clone()),
                    bases_list.clone(),
                    ns_map_val.clone(),
                ],
                Vec::new(),
                Some(new_owner),
                Some(meta_cls_obj),
            )?
        } else {
            let Val::Map(owned_ns) = ns_map_val.clone() else {
                unreachable!();
            };
            self.register_class_from_ns(&meta_name, &name, &bases, owned_ns)?
        };

        if let Some(created_name) = self.class_object_tag(&cls_obj) {
            let actual_meta = self
                .classes
                .get(&created_name)
                .and_then(|c| c.metaclass.clone())
                .unwrap_or_else(|| meta_name.clone());
            if let Some((init_owner, init_id)) = self
                .find_method_with_owner(&actual_meta, "__init__")
                .filter(|(o, _)| o != "type" && o != "object")
            {
                let _ = self.call_fn_id_ctx(
                    init_id,
                    vec![cls_obj.clone(), Val::Str(name), bases_list, ns_map_val],
                    Vec::new(),
                    Some(init_owner),
                    Some(cls_obj.clone()),
                )?;
            }
        }
        Ok(cls_obj)
    }

    fn class_is_a(&self, class_name: &str, target: &str) -> bool {
        if class_name == target || target == "object" {
            return true;
        }
        if let Some(cls) = self.classes.get(class_name) {
            if !cls.mro.is_empty() {
                return cls.mro.iter().any(|c| c == target);
            }
            return cls.bases.iter().any(|b| self.class_is_a(b, target));
        }
        false
    }

    fn find_method_with_owner(&self, class_name: &str, method: &str) -> Option<(String, usize)> {
        let mro = self.class_mro_slice(class_name);
        for name in mro {
            if let Some(cls) = self.classes.get(&name) {
                if let Some(&id) = cls.methods.get(method) {
                    return Some((name, id));
                }
            }
        }
        None
    }

    fn find_method(&self, class_name: &str, method: &str) -> Option<usize> {
        self.find_method_with_owner(class_name, method)
            .map(|(_, id)| id)
    }

    fn find_member_in_mro_slice(
        &self,
        mro: &[String],
        member: &str,
    ) -> Option<(String, bool, Val, Option<usize>)> {
        for cls_name in mro {
            if let Some(cls) = self.classes.get(cls_name) {
                if let Some(v) = cls.attrs.get(member) {
                    return Some((cls_name.clone(), true, v.clone(), None));
                }
                if let Some(&fid) = cls.methods.get(member) {
                    return Some((cls_name.clone(), false, Val::Null, Some(fid)));
                }
            }
        }
        None
    }

    fn value_matches_rt(&self, value: &Val, ty: &str) -> bool {
        match ty {
            "Any" | "object" => true,
            "Number" | "Float" | "float" => matches!(value, Val::Num(_)),
            "Int" | "int" => matches!(value, Val::Num(n) if n.fract() == 0.0),
            "String" | "str" => matches!(value, Val::Str(_)),
            "Bool" | "bool" => matches!(value, Val::Bool(_)),
            "List" | "list" => matches!(value, Val::List(_)),
            "Map" | "dict" => matches!(value, Val::Map(_)),
            "Null" | "Void" | "null" => matches!(value, Val::Null),
            "Object" => matches!(value, Val::Object(_)),
            "Function" | "function" => matches!(value, Val::Fn(_)),
            "Generator" => matches!(value, Val::Generator(_)),
            custom => match value {
                Val::Object(id) => self
                    .objects
                    .get(*id)
                    .map(|o| self.class_is_a(&o.class_name, custom))
                    .unwrap_or(false),
                _ => false,
            },
        }
    }

    fn check_type(&self, value: &Val, ty: &str, kind: &str, name: &str) -> Result<(), Flow> {
        let ok = self.value_matches_rt(value, ty);
        if ok {
            Ok(())
        } else if name.is_empty() {
            Err(Flow::Err(format!(
                "{kind} value expected {ty}, got {}",
                value.show()
            )))
        } else {
            Err(Flow::Err(format!(
                "{kind} '{name}' expected {ty}, got {}",
                value.show()
            )))
        }
    }

    fn extend_kwargs(&self, val: Val, out: &mut Vec<(String, Val)>) -> Result<(), Flow> {
        let Val::Map(pairs) = val else {
            return Err(Flow::Err("**kwargs unpacking expects a Map".into()));
        };
        for (k, v) in pairs {
            let Val::Str(ks) = k else {
                return Err(Flow::Err("**kwargs keys must be Strings".into()));
            };
            out.push((ks, v));
        }
        Ok(())
    }

    fn bind_params_kw(
        &self,
        args: &mut Vec<Val>,
        kwargs: Vec<(String, Val)>,
        params: &[(&str, Option<&str>)],
    ) -> Result<Vec<Val>, Flow> {
        let mut kw_idx = None;
        let mut var_idx = None;
        let mut fixed_end = params.len();
        if let Some((_, Some("**"))) = params.last() {
            kw_idx = Some(params.len() - 1);
            fixed_end -= 1;
        }
        if fixed_end > 0 && params[fixed_end - 1].1 == Some("*") {
            var_idx = Some(fixed_end - 1);
            fixed_end -= 1;
        }
        let mut slots: Vec<Option<Val>> = vec![None; params.len()];
        let mut extra_pos = Vec::new();
        for (i, val) in args.drain(..).enumerate() {
            if i < fixed_end {
                slots[i] = Some(val);
            } else if var_idx.is_some() {
                extra_pos.push(val);
            } else {
                return Err(Flow::Err(format!(
                    "function expects at most {fixed_end} positional arguments"
                )));
            }
        }
        if let Some(vi) = var_idx {
            slots[vi] = Some(Val::List(extra_pos));
        }
        let mut extra_kw: Vec<(Val, Val)> = Vec::new();
        for (k, v) in kwargs {
            if let Some(pos_idx) = params[..fixed_end].iter().position(|(p, _)| *p == k) {
                if slots[pos_idx].is_some() {
                    return Err(Flow::Err(format!(
                        "multiple values for argument '{k}'"
                    )));
                }
                slots[pos_idx] = Some(v);
            } else if kw_idx.is_some() {
                if let Some((_, existing)) = extra_kw
                    .iter_mut()
                    .find(|(ek, _)| *ek == Val::Str(k.clone()))
                {
                    *existing = v;
                } else {
                    extra_kw.push((Val::Str(k), v));
                }
            } else {
                return Err(Flow::Err(format!("unexpected keyword argument '{k}'")));
            }
        }
        for i in 0..fixed_end {
            if slots[i].is_none() {
                if params[i].1 == Some("?default") {
                    slots[i] = Some(Val::Null);
                } else {
                    return Err(Flow::Err(format!(
                        "missing required argument '{}'",
                        params[i].0
                    )));
                }
            }
        }
        if let Some(ki) = kw_idx {
            slots[ki] = Some(Val::Map(extra_kw));
        }
        let resolved: Vec<Val> = slots.into_iter().map(|s| s.unwrap_or(Val::Null)).collect();
        for ((param, ty), value) in params.iter().zip(resolved.iter()) {
            if let Some(expected) = ty {
                if *expected != "*" && *expected != "**" && *expected != "?default" {
                    self.check_type(value, expected, "argument", param)?;
                }
            }
        }
        Ok(resolved)
    }

    fn try_dunder(
        &mut self,
        obj: &Val,
        dunder: &str,
        extra: Vec<Val>,
        kwargs: Vec<(String, Val)>,
    ) -> Result<Option<Val>, Flow> {
        let Val::Object(id) = obj else {
            return Ok(None);
        };
        let class_name = self
            .objects
            .get(*id)
            .ok_or_else(|| Flow::Err("dangling object".into()))?
            .class_name
            .clone();
        let Some((owner, mid)) = self.find_method_with_owner(&class_name, dunder) else {
            return Ok(None);
        };
        let mut args = vec![Val::Object(*id)];
        args.extend(extra);
        Ok(Some(self.call_fn_id_ctx(
            mid,
            args,
            kwargs,
            Some(owner),
            Some(Val::Object(*id)),
        )?))
    }

    fn show_val_runtime(&mut self, val: &Val) -> Result<String, Flow> {
        if let Some(res) = self.try_dunder(val, "__str__", vec![], vec![])? {
            return Ok(res.show());
        }
        if is_py_handle_id(val).is_some() {
            return self.py_str(val);
        }
        Ok(val.show())
    }

    fn repr_val_runtime(&mut self, val: &Val) -> Result<String, Flow> {
        if let Some(res) = self.try_dunder(val, "__repr__", vec![], vec![])? {
            return Ok(res.show());
        }
        if is_py_handle_id(val).is_some() {
            return self.py_repr(val);
        }
        match val {
            Val::Str(s) => Ok(format!("{s:?}")),
            _ => self.show_val_runtime(val),
        }
    }

    fn call_fn_id(&mut self, id: usize, args: Vec<Val>) -> Result<Val, Flow> {
        self.call_fn_id_kw(id, args, vec![])
    }

    fn call_fn_id_kw(
        &mut self,
        id: usize,
        args: Vec<Val>,
        kwargs: Vec<(String, Val)>,
    ) -> Result<Val, Flow> {
        self.call_fn_id_ctx(id, args, kwargs, None, None)
    }

    fn call_fn_id_ctx(
        &mut self,
        id: usize,
        args: Vec<Val>,
        kwargs: Vec<(String, Val)>,
        active_class: Option<String>,
        active_self: Option<Val>,
    ) -> Result<Val, Flow> {
        let f = self
            .fns
            .get(id)
            .cloned()
            .ok_or_else(|| Flow::Err(format!("dangling function id {id}")))?;
        self.call_ctx_stack.push((active_class, active_self));
        let res = f(self, args, kwargs);
        self.call_ctx_stack.pop();
        res
    }

    fn call_val(&mut self, val: &Val, args: Vec<Val>) -> Result<Val, Flow> {
        self.call_val_kw(val, args, vec![])
    }

    fn call_val_kw(
        &mut self,
        val: &Val,
        args: Vec<Val>,
        kwargs: Vec<(String, Val)>,
    ) -> Result<Val, Flow> {
        match val {
            Val::Fn(id) => self.call_fn_id_kw(*id, args, kwargs),
            Val::Object(id) => {
                let Some(snap) = self.objects.get(*id).map(|o| (o.class_name.clone(), o.fields.clone())) else {
                    return Err(Flow::Err("dangling object".into()));
                };
                if snap.0 == "__bound_method__" {
                    let func = snap.1.get("__func__").cloned().ok_or_else(|| Flow::Err("corrupt bound method".into()))?;
                    let bself = snap.1.get("__self__").cloned().ok_or_else(|| Flow::Err("corrupt bound method".into()))?;
                    let owner = snap.1.get("__owner__").map(|v| v.show());
                    let mut full_args = Vec::with_capacity(1 + args.len());
                    full_args.push(bself.clone());
                    full_args.extend(args);
                    return match func {
                        Val::Fn(fid) => self.call_fn_id_ctx(fid, full_args, kwargs, owner, Some(bself)),
                        _ => self.call_val_kw(&func, full_args, kwargs),
                    };
                }
                if let Some(Val::Str(ref cls_name)) = snap.1.get("__kovrek_class__") {
                    return self.instantiate_kw(cls_name, args, kwargs);
                }
                if let Some(res) = self.try_dunder(val, "__call__", args, kwargs)? {
                    Ok(res)
                } else {
                    Err(Flow::Err("object is not callable (missing __call__)".into()))
                }
            }
            Val::Map(_) if is_py_handle_id(val).is_some() => self.py_call(val, &args, &kwargs),
            Val::Str(name) => {
                if let Some(&id) = self.named_fns.get(name) {
                    self.call_fn_id_kw(id, args, kwargs)
                } else {
                    Err(Flow::Err(format!("unknown function '{name}'")))
                }
            }
            _ => Err(Flow::Err("value is not callable".into())),
        }
    }

    fn instantiate_kw(
        &mut self,
        class_name: &str,
        args: Vec<Val>,
        kwargs: Vec<(String, Val)>,
    ) -> Result<Val, Flow> {
        match class_name {
            "int" | "float" | "str" | "bool" | "list" | "dict" | "type" => {
                return self
                    .call_builtin_kw(class_name, &args, &kwargs)?
                    .ok_or_else(|| Flow::Err(format!("builtin '{class_name}' returned no value")));
            }
            "object" => {
                if !args.is_empty() || !kwargs.is_empty() {
                    return Err(Flow::Err("object() takes no arguments".into()));
                }
                return Ok(self.alloc_object("object", HashMap::new()));
            }
            _ => {}
        }
        let meta_name = self
            .classes
            .get(class_name)
            .ok_or_else(|| Flow::Err(format!("unknown class '{class_name}'")))?
            .metaclass
            .clone();
        if let Some(meta) = meta_name {
            if let Some((owner, call_id)) = self.find_method_with_owner(&meta, "__call__") {
                if owner != "type" && owner != "object" {
                    let cls_obj = self.class_object_val(class_name)?;
                    let mut call_args = Vec::with_capacity(1 + args.len());
                    call_args.push(cls_obj.clone());
                    call_args.extend(args);
                    return self.call_fn_id_ctx(
                        call_id,
                        call_args,
                        kwargs,
                        Some(owner),
                        Some(cls_obj),
                    );
                }
            }
        }
        self.default_instantiate_kw(class_name, args, kwargs)
    }

    fn default_instantiate_kw(
        &mut self,
        class_name: &str,
        args: Vec<Val>,
        kwargs: Vec<(String, Val)>,
    ) -> Result<Val, Flow> {
        let mro = self.class_mro_slice(class_name);
        let mut custom_new: Option<(String, usize)> = None;
        for c in &mro {
            if c == "object" || c == "type" {
                continue;
            }
            if let Some(cls) = self.classes.get(c) {
                if let Some(&fid) = cls.methods.get("__new__") {
                    custom_new = Some((c.clone(), fid));
                    break;
                }
            }
        }
        let mut custom_new_ran = false;
        let inst = if let Some((new_owner, new_id)) = custom_new {
            custom_new_ran = true;
            let cls_obj = self.class_object_val(class_name)?;
            let mut new_args = Vec::with_capacity(1 + args.len());
            new_args.push(cls_obj.clone());
            new_args.extend(args.clone());
            let created = self.call_fn_id_ctx(
                new_id,
                new_args,
                kwargs.clone(),
                Some(new_owner),
                Some(cls_obj),
            )?;
            let is_instance = match created {
                Val::Object(id) => self
                    .objects
                    .get(id)
                    .map(|o| self.class_is_a(&o.class_name, class_name))
                    .unwrap_or(false),
                _ => false,
            };
            if !is_instance {
                return Ok(created);
            }
            created
        } else {
            self.alloc_object(class_name, HashMap::new())
        };

        let mut init_found: Option<(String, usize)> = None;
        for c in &mro {
            if c == "object" || c == "type" {
                continue;
            }
            if let Some(cls) = self.classes.get(c) {
                if let Some(&fid) = cls.methods.get("__init__").or_else(|| cls.methods.get("init"))
                {
                    init_found = Some((c.clone(), fid));
                    break;
                }
            }
        }
        if let Some((init_owner, init_id)) = init_found {
            let mut call_args = vec![inst.clone()];
            call_args.extend(args);
            let _ = self.call_fn_id_ctx(
                init_id,
                call_args,
                kwargs,
                Some(init_owner),
                Some(inst.clone()),
            )?;
        } else if !custom_new_ran && (!args.is_empty() || !kwargs.is_empty()) {
            return Err(Flow::Err(format!("{class_name} has no init method")));
        }
        Ok(inst)
    }

    #[inline(always)]
    fn call_target(
        &mut self,
        name: &str,
        local_val: Option<Val>,
        args: Vec<Val>,
    ) -> Result<Val, Flow> {
        self.call_target_kw(name, local_val, args, vec![])
    }

    #[inline(always)]
    fn call_target_kw(
        &mut self,
        name: &str,
        local_val: Option<Val>,
        args: Vec<Val>,
        kwargs: Vec<(String, Val)>,
    ) -> Result<Val, Flow> {
        if let Some(res) = self.call_builtin_kw(name, &args, &kwargs)? {
            return Ok(res);
        }
        if let Some(lv) = local_val {
            return self.call_val_kw(&lv, args, kwargs);
        }
        if let Some(gv) = self.globals.get(name).cloned() {
            return self.call_val_kw(&gv, args, kwargs);
        }
        if self.classes.contains_key(name) {
            return self.instantiate_kw(name, args, kwargs);
        }
        if let Some(&id) = self.named_fns.get(name) {
            return self.call_fn_id_kw(id, args, kwargs);
        }
        Err(Flow::Err(format!("unknown function or class '{name}'")))
    }

    #[inline(always)]
    fn neg(&self, v: Val) -> Result<Val, Flow> {
        match v {
            Val::Num(n) => Ok(Val::Num(-n)),
            _ => Err(Flow::Err("unary '-' requires number".into())),
        }
    }

    #[inline(always)]
    fn bin_add(&mut self, a: Val, b: Val) -> Result<Val, Flow> {
        if let Some(res) = self.try_dunder(&a, "__add__", vec![b.clone()], vec![])? {
            return Ok(res);
        }
        match (a, b) {
            (Val::Num(x), Val::Num(y)) => Ok(Val::Num(x + y)),
            (Val::Str(x), Val::Str(y)) => Ok(Val::Str(x + &y)),
            (Val::List(mut x), Val::List(y)) => {
                x.extend(y);
                Ok(Val::List(x))
            }
            _ => Err(Flow::Err("invalid '+' operands".into())),
        }
    }

    #[inline(always)]
    fn bin_sub(&mut self, a: Val, b: Val) -> Result<Val, Flow> {
        if let Some(res) = self.try_dunder(&a, "__sub__", vec![b.clone()], vec![])? {
            return Ok(res);
        }
        match (a, b) {
            (Val::Num(x), Val::Num(y)) => Ok(Val::Num(x - y)),
            _ => Err(Flow::Err("numeric operation requires numbers".into())),
        }
    }

    #[inline(always)]
    fn bin_mul(&mut self, a: Val, b: Val) -> Result<Val, Flow> {
        if let Some(res) = self.try_dunder(&a, "__mul__", vec![b.clone()], vec![])? {
            return Ok(res);
        }
        match (a, b) {
            (Val::Num(x), Val::Num(y)) => Ok(Val::Num(x * y)),
            (Val::Str(s), Val::Num(n)) | (Val::Num(n), Val::Str(s)) => {
                Ok(Val::Str(s.repeat(n.max(0.0) as usize)))
            }
            (Val::List(items), Val::Num(n)) | (Val::Num(n), Val::List(items)) => {
                let count = n.max(0.0) as usize;
                let mut out = Vec::with_capacity(items.len() * count);
                for _ in 0..count {
                    out.extend(items.iter().cloned());
                }
                Ok(Val::List(out))
            }
            _ => Err(Flow::Err("numeric operation requires numbers".into())),
        }
    }

    #[inline(always)]
    fn bin_div(&self, a: Val, b: Val) -> Result<Val, Flow> {
        match (a, b) {
            (Val::Num(x), Val::Num(y)) => Ok(Val::Num(x / y)),
            _ => Err(Flow::Err("numeric operation requires numbers".into())),
        }
    }

    #[inline(always)]
    fn bin_mod(&self, a: Val, b: Val) -> Result<Val, Flow> {
        match (a, b) {
            (Val::Num(x), Val::Num(y)) => Ok(Val::Num(x % y)),
            _ => Err(Flow::Err("numeric operation requires numbers".into())),
        }
    }

    #[inline(always)]
    fn bin_pow(&self, a: Val, b: Val) -> Result<Val, Flow> {
        match (a, b) {
            (Val::Num(x), Val::Num(y)) => Ok(Val::Num(x.powf(y))),
            _ => Err(Flow::Err("numeric operation requires numbers".into())),
        }
    }

    #[inline(always)]
    fn bin_eq(&mut self, a: Val, b: Val) -> Result<Val, Flow> {
        if let Some(res) = self.try_dunder(&a, "__eq__", vec![b.clone()], vec![])? {
            return Ok(Val::Bool(res.truthy()));
        }
        Ok(Val::Bool(a == b))
    }

    #[inline(always)]
    fn bin_ne(&mut self, a: Val, b: Val) -> Result<Val, Flow> {
        Ok(Val::Bool(!self.bin_eq(a, b)?.truthy()))
    }

    #[inline(always)]
    fn bin_lt(&self, a: Val, b: Val) -> Result<Val, Flow> {
        match (a, b) {
            (Val::Num(x), Val::Num(y)) => Ok(Val::Bool(x < y)),
            (Val::Str(x), Val::Str(y)) => Ok(Val::Bool(x < y)),
            _ => Err(Flow::Err("comparison requires numbers".into())),
        }
    }

    #[inline(always)]
    fn bin_le(&self, a: Val, b: Val) -> Result<Val, Flow> {
        match (a, b) {
            (Val::Num(x), Val::Num(y)) => Ok(Val::Bool(x <= y)),
            (Val::Str(x), Val::Str(y)) => Ok(Val::Bool(x <= y)),
            _ => Err(Flow::Err("comparison requires numbers".into())),
        }
    }

    #[inline(always)]
    fn bin_gt(&self, a: Val, b: Val) -> Result<Val, Flow> {
        match (a, b) {
            (Val::Num(x), Val::Num(y)) => Ok(Val::Bool(x > y)),
            (Val::Str(x), Val::Str(y)) => Ok(Val::Bool(x > y)),
            _ => Err(Flow::Err("comparison requires numbers".into())),
        }
    }

    #[inline(always)]
    fn bin_ge(&self, a: Val, b: Val) -> Result<Val, Flow> {
        match (a, b) {
            (Val::Num(x), Val::Num(y)) => Ok(Val::Bool(x >= y)),
            (Val::Str(x), Val::Str(y)) => Ok(Val::Bool(x >= y)),
            _ => Err(Flow::Err("comparison requires numbers".into())),
        }
    }

    fn val_index(&mut self, obj: Val, key: Val) -> Result<Val, Flow> {
        if is_py_handle_id(&obj).is_some() {
            return self.py_getitem(&obj, &key);
        }
        if let Some(res) = self.try_dunder(&obj, "__getitem__", vec![key.clone()], vec![])? {
            return Ok(res);
        }
        match (obj, key) {
            (Val::List(v), Val::Num(n)) => {
                let raw = n as isize;
                let len = v.len() as isize;
                let resolved = if raw < 0 { len + raw } else { raw };
                if resolved < 0 || (resolved as usize) >= v.len() {
                    return Err(Flow::Err(format!("list index {raw} out of range")));
                }
                Ok(v[resolved as usize].clone())
            }
            (Val::Str(s), Val::Num(n)) => {
                let chars: Vec<char> = s.chars().collect();
                let raw = n as isize;
                let len = chars.len() as isize;
                let resolved = if raw < 0 { len + raw } else { raw };
                if resolved < 0 || (resolved as usize) >= chars.len() {
                    return Err(Flow::Err(format!("string index {raw} out of range")));
                }
                Ok(Val::Str(chars[resolved as usize].to_string()))
            }
            (Val::Map(pairs), k) => pairs
                .iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| v.clone())
                .ok_or_else(|| Flow::Err("map key not found".into())),
            _ => Err(Flow::Err("value is not indexable".into())),
        }
    }

    fn val_set_index(&mut self, obj: Val, key: Val, value: Val) -> Result<Val, Flow> {
        if is_py_handle_id(&obj).is_some() {
            let _ = self.py_setitem(&obj, &key, &value)?;
            return Ok(obj);
        }
        if self
            .try_dunder(&obj, "__setitem__", vec![key.clone(), value.clone()], vec![])?
            .is_some()
        {
            return Ok(obj);
        }
        match (obj, key) {
            (Val::List(mut items), Val::Num(n)) => {
                if n.fract() != 0.0 {
                    return Err(Flow::Err("list index must be integer".into()));
                }
                let raw = n as isize;
                let len = items.len() as isize;
                let resolved = if raw < 0 { len + raw } else { raw };
                if resolved < 0 || (resolved as usize) >= items.len() {
                    return Err(Flow::Err(format!("list index {raw} out of range")));
                }
                items[resolved as usize] = value;
                Ok(Val::List(items))
            }
            (Val::Map(mut pairs), k) => {
                if let Some((_, existing)) = pairs.iter_mut().find(|(ek, _)| *ek == k) {
                    *existing = value;
                } else {
                    pairs.push((k, value));
                }
                Ok(Val::Map(pairs))
            }
            _ => Err(Flow::Err("index assignment requires List or Map".into())),
        }
    }

    fn descriptor_has_method(&self, val: &Val, method: &str) -> bool {
        let Val::Object(id) = val else {
            return false;
        };
        let Some(cls) = self.objects.get(*id).map(|o| o.class_name.clone()) else {
            return false;
        };
        if cls == "__property__" {
            return true;
        }
        if matches!(
            cls.as_str(),
            "__staticmethod__" | "__classmethod__" | "__bound_method__" | "__super__"
        ) {
            return false;
        }
        self.find_method_with_owner(&cls, method).is_some()
    }

    fn materialize_member(
        &mut self,
        owner: &str,
        is_attr: bool,
        attr_val: Val,
        method_id: Option<usize>,
        recv_opt: Option<Val>,
        is_cls_bind: bool,
    ) -> Result<Val, Flow> {
        if is_attr {
            if let Val::Object(did) = attr_val {
                let dcls = self
                    .objects
                    .get(did)
                    .ok_or_else(|| Flow::Err("dangling object".into()))?
                    .class_name
                    .clone();
                if dcls == "__staticmethod__" {
                    return self.objects[did]
                        .fields
                        .get("__func__")
                        .cloned()
                        .ok_or_else(|| Flow::Err("corrupt staticmethod".into()));
                }
                if dcls == "__classmethod__" {
                    let func = self.objects[did]
                        .fields
                        .get("__func__")
                        .cloned()
                        .ok_or_else(|| Flow::Err("corrupt classmethod".into()))?;
                    let cls_val = match recv_opt {
                        Some(ref r) => {
                            if is_cls_bind {
                                r.clone()
                            } else if let Val::Object(oid) = r {
                                let ocls = self.objects[*oid].class_name.clone();
                                self.class_object_val(&ocls)?
                            } else {
                                self.class_object_val(owner)?
                            }
                        }
                        None => self.class_object_val(owner)?,
                    };
                    return Ok(self.make_bound_method(func, cls_val, Some(owner)));
                }
                if dcls == "__property__" {
                    if !is_cls_bind {
                        if let Some(inst) = recv_opt {
                            let fget = self.objects[did]
                                .fields
                                .get("fget")
                                .cloned()
                                .unwrap_or(Val::Null);
                            if matches!(fget, Val::Null) {
                                return Err(Flow::Err(
                                    "unreadable attribute (property has no getter)".into(),
                                ));
                            }
                            return match fget {
                                Val::Fn(fid) => self.call_fn_id_ctx(
                                    fid,
                                    vec![inst.clone()],
                                    vec![],
                                    Some(owner.to_string()),
                                    Some(inst),
                                ),
                                _ => self.call_val(&fget, vec![inst]),
                            };
                        }
                    }
                    return Ok(attr_val);
                }
                if let Some((_, get_id)) = self.find_method_with_owner(&dcls, "__get__") {
                    let owner_cls = match recv_opt {
                        Some(ref r) if is_cls_bind => r.clone(),
                        Some(Val::Object(oid)) => {
                            let ocls = self.objects[oid].class_name.clone();
                            self.class_object_val(&ocls)?
                        }
                        _ => self.class_object_val(owner)?,
                    };
                    let inst_arg = if is_cls_bind {
                        Val::Null
                    } else {
                        recv_opt.unwrap_or(Val::Null)
                    };
                    return self.call_fn_id_ctx(
                        get_id,
                        vec![attr_val, inst_arg, owner_cls],
                        vec![],
                        Some(dcls),
                        Some(Val::Object(did)),
                    );
                }
            }
            return Ok(attr_val);
        }
        let fid = method_id.ok_or_else(|| Flow::Err("missing method id".into()))?;
        let fn_val = Val::Fn(fid);
        if is_cls_bind {
            return Ok(fn_val);
        }
        match recv_opt {
            Some(inst) => Ok(self.make_bound_method(fn_val, inst, Some(owner))),
            None => Ok(fn_val),
        }
    }

    fn super_target_info(&self, super_id: usize) -> Result<(Vec<String>, Val, bool), Flow> {
        let snap = self
            .objects
            .get(super_id)
            .ok_or_else(|| Flow::Err("dangling super object".into()))?;
        let Some(Val::Str(thisclass)) = snap.fields.get("__thisclass__").cloned() else {
            return Err(Flow::Err("corrupt super() object".into()));
        };
        let self_val = snap
            .fields
            .get("__self__")
            .cloned()
            .ok_or_else(|| Flow::Err("corrupt super() object".into()))?;
        let (target_cls, is_cls_bind) = if let Some(c) = self.class_object_tag(&self_val) {
            if self.class_is_a(&c, &thisclass) {
                (c, true)
            } else if let Val::Object(oid) = self_val {
                let oc = self
                    .objects
                    .get(oid)
                    .ok_or_else(|| Flow::Err("dangling object".into()))?
                    .class_name
                    .clone();
                (oc, false)
            } else {
                (c, true)
            }
        } else if let Val::Object(oid) = self_val {
            let oc = self
                .objects
                .get(oid)
                .ok_or_else(|| Flow::Err("dangling object".into()))?
                .class_name
                .clone();
            (oc, false)
        } else {
            return Err(Flow::Err("super() second argument must be object or class".into()));
        };
        let mro = self.class_mro_slice(&target_cls);
        let pos = mro
            .iter()
            .position(|c| c == &thisclass)
            .ok_or_else(|| Flow::Err(format!("'{thisclass}' is not in MRO of '{target_cls}'")))?;
        Ok((mro[pos + 1..].to_vec(), self_val, is_cls_bind))
    }

    fn super_get_attr(&mut self, super_id: usize, name: &str) -> Result<Val, Flow> {
        let (tail_mro, self_val, is_cls_bind) = self.super_target_info(super_id)?;
        if let Some((owner, is_attr, attr_val, method_id)) =
            self.find_member_in_mro_slice(&tail_mro, name)
        {
            return self.materialize_member(
                &owner,
                is_attr,
                attr_val,
                method_id,
                Some(self_val),
                is_cls_bind,
            );
        }
        let is_class_arg = self.class_object_tag(&self_val).is_some();
        for c in &tail_mro {
            if c == "object" {
                if let Some(b) = match name {
                    "__new__" => Some("__object_new__"),
                    "__init__" => Some("__object_init__"),
                    "__setattr__" => Some("__object_setattr__"),
                    "__delattr__" => Some("__object_delattr__"),
                    _ => None,
                } {
                    let fnv = self.builtin_fn_val(b);
                    return Ok(self.make_bound_method(fnv, self_val, Some("object")));
                }
            } else if c == "type" && (is_cls_bind || is_class_arg) {
                if let Some(b) = match name {
                    "__call__" => Some("__type_call__"),
                    "__new__" => Some("__type_new__"),
                    "__init__" => Some("__type_init__"),
                    _ => None,
                } {
                    let fnv = self.builtin_fn_val(b);
                    if name == "__new__" {
                        return Ok(fnv);
                    }
                    return Ok(self.make_bound_method(fnv, self_val, Some("type")));
                }
            }
        }
        Err(Flow::Err(format!("'super' object has no attribute '{name}'")))
    }

    fn get_member(&mut self, obj: Val, name: &str) -> Result<Val, Flow> {
        if is_py_handle_id(&obj).is_some() {
            return self.py_get(&obj, name);
        }
        let Val::Object(id) = obj else {
            if let Val::Map(pairs) = obj {
                return pairs
                    .iter()
                    .find(|(k, _)| *k == Val::Str(name.to_string()))
                    .map(|(_, v)| v.clone())
                    .ok_or_else(|| Flow::Err(format!("undefined member '{name}'")));
            }
            return Err(Flow::Err("member access requires object or map".into()));
        };
        let (cls_name, fields) = {
            let data = self
                .objects
                .get(id)
                .ok_or_else(|| Flow::Err("dangling object".into()))?;
            (data.class_name.clone(), data.fields.clone())
        };
        if cls_name == "__super__" {
            return self.super_get_attr(id, name);
        }
        if cls_name == "__property__" {
            if matches!(name, "getter" | "setter" | "deleter") {
                let bname = match name {
                    "getter" => "__prop_getter__",
                    "setter" => "__prop_setter__",
                    _ => "__prop_deleter__",
                };
                let fnv = self.builtin_fn_val(bname);
                return Ok(self.make_bound_method(fnv, Val::Object(id), None));
            }
            if let Some(v) = fields.get(name) {
                return Ok(v.clone());
            }
            return Err(Flow::Err(format!("'property' object has no attribute '{name}'")));
        }
        if let Some(Val::Str(ref cname)) = fields.get("__kovrek_class__") {
            match name {
                "__name__" => return Ok(Val::Str(cname.clone())),
                "__bases__" => {
                    let bases = self
                        .classes
                        .get(cname)
                        .map(|c| c.bases.clone())
                        .unwrap_or_default();
                    let mut out = Vec::with_capacity(bases.len());
                    for b in &bases {
                        out.push(self.class_object_val(b)?);
                    }
                    return Ok(Val::List(out));
                }
                "__mro__" => {
                    let mro = self.class_mro_slice(cname);
                    let mut out = Vec::with_capacity(mro.len());
                    for c in &mro {
                        out.push(self.class_object_val(c)?);
                    }
                    return Ok(Val::List(out));
                }
                "__dict__" => {
                    let pairs = self
                        .classes
                        .get(cname)
                        .map(|c| {
                            c.attrs
                                .iter()
                                .map(|(k, v)| (Val::Str(k.clone()), v.clone()))
                                .collect()
                        })
                        .unwrap_or_default();
                    return Ok(Val::Map(pairs));
                }
                _ => {}
            }
            let mro = self.class_mro_slice(cname);
            if let Some((owner, is_attr, attr_val, method_id)) =
                self.find_member_in_mro_slice(&mro, name)
            {
                return self.materialize_member(
                    &owner,
                    is_attr,
                    attr_val,
                    method_id,
                    Some(Val::Object(id)),
                    true,
                );
            }
            if cname == "object" {
                if let Some(b) = match name {
                    "__new__" => Some("__object_new__"),
                    "__init__" => Some("__object_init__"),
                    "__setattr__" => Some("__object_setattr__"),
                    "__delattr__" => Some("__object_delattr__"),
                    _ => None,
                } {
                    return Ok(self.builtin_fn_val(b));
                }
            } else if cname == "type" {
                if let Some(b) = match name {
                    "__new__" => Some("__type_new__"),
                    "__init__" => Some("__type_init__"),
                    "__call__" => Some("__type_call__"),
                    _ => None,
                } {
                    return Ok(self.builtin_fn_val(b));
                }
            }
            let meta_mro = self.class_mro_slice(&cls_name);
            if let Some((owner, is_attr, attr_val, method_id)) =
                self.find_member_in_mro_slice(&meta_mro, name)
            {
                return self.materialize_member(
                    &owner,
                    is_attr,
                    attr_val,
                    method_id,
                    Some(Val::Object(id)),
                    false,
                );
            }
            return Err(Flow::Err(format!("type object '{cname}' has no attribute '{name}'")));
        }

        let mro = self.class_mro_slice(&cls_name);
        let hit = self.find_member_in_mro_slice(&mro, name);
        if let Some((ref owner, true, ref attr_val, _)) = hit {
            if self.descriptor_has_method(attr_val, "__set__")
                || self.descriptor_has_method(attr_val, "__delete__")
            {
                return self.materialize_member(
                    owner,
                    true,
                    attr_val.clone(),
                    None,
                    Some(Val::Object(id)),
                    false,
                );
            }
        }
        if let Some(v) = fields.get(name) {
            return Ok(v.clone());
        }
        if let Some((owner, is_attr, attr_val, method_id)) = hit {
            return self.materialize_member(
                &owner,
                is_attr,
                attr_val,
                method_id,
                Some(Val::Object(id)),
                false,
            );
        }
        if name != "__getattr__" {
            if let Some((owner, ga_id)) = self.find_method_with_owner(&cls_name, "__getattr__") {
                return self.call_fn_id_ctx(
                    ga_id,
                    vec![Val::Object(id), Val::Str(name.to_string())],
                    vec![],
                    Some(owner),
                    Some(Val::Object(id)),
                );
            }
        }
        Err(Flow::Err(format!("undefined field '{name}'")))
    }

    fn set_member(&mut self, obj: Val, name: &str, value: Val) -> Result<(), Flow> {
        if is_py_handle_id(&obj).is_some() {
            let _ = self.py_set(&obj, name, &value)?;
            return Ok(());
        }
        let Val::Object(id) = obj else {
            return Err(Flow::Err("member assignment requires object".into()));
        };
        let (cls_name, cls_tag) = {
            let data = self
                .objects
                .get(id)
                .ok_or_else(|| Flow::Err("dangling object".into()))?;
            (
                data.class_name.clone(),
                match data.fields.get("__kovrek_class__") {
                    Some(Val::Str(s)) => Some(s.clone()),
                    _ => None,
                },
            )
        };
        if let Some(cname) = cls_tag {
            let is_builtin = match value {
                Val::Fn(fid) => self.builtin_fn_ids.values().any(|&b| b == fid),
                _ => false,
            };
            let cls = self
                .classes
                .get_mut(&cname)
                .ok_or_else(|| Flow::Err(format!("unknown class '{cname}'")))?;
            cls.attrs.remove(name);
            cls.methods.remove(name);
            if let Val::Fn(fid) = value {
                if !is_builtin {
                    cls.methods.insert(name.to_string(), fid);
                    return Ok(());
                }
            }
            cls.attrs.insert(name.to_string(), value);
            return Ok(());
        }
        if name != "__setattr__" {
            if let Some((owner, sa_id)) = self.find_method_with_owner(&cls_name, "__setattr__") {
                if owner != "object" && owner != "type" {
                    let _ = self.call_fn_id_ctx(
                        sa_id,
                        vec![Val::Object(id), Val::Str(name.to_string()), value],
                        vec![],
                        Some(owner),
                        Some(Val::Object(id)),
                    )?;
                    return Ok(());
                }
            }
        }
        let mro = self.class_mro_slice(&cls_name);
        if let Some((owner, true, attr_val, _)) = self.find_member_in_mro_slice(&mro, name) {
            if let Val::Object(did) = attr_val {
                let dcls = self.objects[did].class_name.clone();
                if dcls == "__property__" {
                    let fset = self.objects[did]
                        .fields
                        .get("fset")
                        .cloned()
                        .unwrap_or(Val::Null);
                    if matches!(fset, Val::Null) {
                        return Err(Flow::Err(format!("property '{name}' has no setter")));
                    }
                    let _ = match fset {
                        Val::Fn(fid) => self.call_fn_id_ctx(
                            fid,
                            vec![Val::Object(id), value],
                            vec![],
                            Some(owner),
                            Some(Val::Object(id)),
                        )?,
                        _ => self.call_val(&fset, vec![Val::Object(id), value])?,
                    };
                    return Ok(());
                }
                if let Some((_, set_id)) = self.find_method_with_owner(&dcls, "__set__") {
                    let _ = self.call_fn_id_ctx(
                        set_id,
                        vec![attr_val, Val::Object(id), value],
                        vec![],
                        Some(dcls),
                        Some(Val::Object(did)),
                    )?;
                    return Ok(());
                }
            }
        }
        let data = self
            .objects
            .get_mut(id)
            .ok_or_else(|| Flow::Err("dangling object".into()))?;
        data.fields.insert(name.to_string(), value);
        Ok(())
    }

    fn del_member(&mut self, obj: Val, name: &str) -> Result<(), Flow> {
        let Val::Object(id) = obj else {
            return Err(Flow::Err("delattr requires Object".into()));
        };
        let (cls_name, cls_tag) = {
            let data = self
                .objects
                .get(id)
                .ok_or_else(|| Flow::Err("dangling object".into()))?;
            (
                data.class_name.clone(),
                match data.fields.get("__kovrek_class__") {
                    Some(Val::Str(s)) => Some(s.clone()),
                    _ => None,
                },
            )
        };
        if let Some(cname) = cls_tag {
            let cls = self
                .classes
                .get_mut(&cname)
                .ok_or_else(|| Flow::Err(format!("unknown class '{cname}'")))?;
            let removed = cls.attrs.remove(name).is_some() || cls.methods.remove(name).is_some();
            if removed {
                return Ok(());
            }
            return Err(Flow::Err(format!("type object '{cname}' has no attribute '{name}'")));
        }
        let mro = self.class_mro_slice(&cls_name);
        if let Some((owner, true, attr_val, _)) = self.find_member_in_mro_slice(&mro, name) {
            if let Val::Object(did) = attr_val {
                let dcls = self.objects[did].class_name.clone();
                if dcls == "__property__" {
                    let fdel = self.objects[did]
                        .fields
                        .get("fdel")
                        .cloned()
                        .unwrap_or(Val::Null);
                    if matches!(fdel, Val::Null) {
                        return Err(Flow::Err(format!("property '{name}' has no deleter")));
                    }
                    let _ = match fdel {
                        Val::Fn(fid) => self.call_fn_id_ctx(
                            fid,
                            vec![Val::Object(id)],
                            vec![],
                            Some(owner),
                            Some(Val::Object(id)),
                        )?,
                        _ => self.call_val(&fdel, vec![Val::Object(id)])?,
                    };
                    return Ok(());
                }
                if let Some((_, del_id)) = self.find_method_with_owner(&dcls, "__delete__") {
                    let _ = self.call_fn_id_ctx(
                        del_id,
                        vec![attr_val, Val::Object(id)],
                        vec![],
                        Some(dcls),
                        Some(Val::Object(did)),
                    )?;
                    return Ok(());
                }
            }
        }
        if name != "__delattr__" {
            if let Some((owner, da_id)) = self.find_method_with_owner(&cls_name, "__delattr__") {
                if owner != "object" && owner != "type" {
                    let _ = self.call_fn_id_ctx(
                        da_id,
                        vec![Val::Object(id), Val::Str(name.to_string())],
                        vec![],
                        Some(owner),
                        Some(Val::Object(id)),
                    )?;
                    return Ok(());
                }
            }
        }
        let data = self
            .objects
            .get_mut(id)
            .ok_or_else(|| Flow::Err("dangling object".into()))?;
        data.fields
            .remove(name)
            .ok_or_else(|| Flow::Err(format!("undefined field '{name}'")))?;
        Ok(())
    }

    fn call_builtin_wrapper(
        &mut self,
        bname: &str,
        args: Vec<Val>,
        kwargs: Vec<(String, Val)>,
    ) -> Result<Val, Flow> {
        match bname {
            "__prop_getter__" | "__prop_setter__" | "__prop_deleter__" => {
                if args.len() != 2 {
                    return Err(Flow::Err(format!("property.{bname} expects 1 callable argument")));
                }
                let Val::Object(pid) = args[0] else {
                    return Err(Flow::Err("property modifier target must be property".into()));
                };
                let mut fields = self.objects[pid].fields.clone();
                let key = match bname {
                    "__prop_getter__" => "fget",
                    "__prop_setter__" => "fset",
                    _ => "fdel",
                };
                fields.insert(key.to_string(), args[1].clone());
                Ok(self.alloc_object("__property__", fields))
            }
            "__object_new__" => {
                let cls_arg = args
                    .first()
                    .ok_or_else(|| Flow::Err("object.__new__(cls) requires a class argument".into()))?;
                let cls_name = self.resolve_class_name_arg(cls_arg)?;
                Ok(self.alloc_object(&cls_name, HashMap::new()))
            }
            "__object_init__" => Ok(Val::Null),
            "__object_setattr__" => {
                if args.len() != 3 {
                    return Err(Flow::Err("object.__setattr__ expects (self, name, value)".into()));
                }
                let Val::Object(id) = args[0] else {
                    return Err(Flow::Err("object.__setattr__ target must be an Object".into()));
                };
                let Val::Str(ref attr) = args[1] else {
                    return Err(Flow::Err("attribute name must be String".into()));
                };
                self.objects[id].fields.insert(attr.clone(), args[2].clone());
                Ok(Val::Null)
            }
            "__object_delattr__" => {
                if args.len() != 2 {
                    return Err(Flow::Err("object.__delattr__ expects (self, name)".into()));
                }
                let Val::Object(id) = args[0] else {
                    return Err(Flow::Err("object.__delattr__ target must be an Object".into()));
                };
                let Val::Str(ref attr) = args[1] else {
                    return Err(Flow::Err("attribute name must be String".into()));
                };
                self.objects[id]
                    .fields
                    .remove(attr)
                    .ok_or_else(|| Flow::Err(format!("undefined field '{attr}'")))?;
                Ok(Val::Null)
            }
            "__type_call__" => {
                if args.is_empty() {
                    return Err(Flow::Err("type.__call__ expects (cls, *args)".into()));
                }
                let cls_name = self.resolve_class_name_arg(&args[0])?;
                self.default_instantiate_kw(&cls_name, args[1..].to_vec(), kwargs)
            }
            "__type_new__" => {
                if args.len() != 4 {
                    return Err(Flow::Err("type.__new__ expects (mcs, name, bases, namespace)".into()));
                }
                let meta_name = self.resolve_class_name_arg(&args[0])?;
                let Val::Str(ref cls_name) = args[1] else {
                    return Err(Flow::Err("type.__new__ name must be String".into()));
                };
                let base_vals = self.to_iter(args[2].clone())?;
                let mut bases = Vec::with_capacity(base_vals.len());
                for b in &base_vals {
                    bases.push(self.resolve_class_name_arg(b)?);
                }
                let Val::Map(ref pairs) = args[3] else {
                    return Err(Flow::Err("type.__new__ namespace must be Map".into()));
                };
                self.register_class_from_ns(&meta_name, cls_name, &bases, pairs.clone())
            }
            "__type_init__" => Ok(Val::Null),
            other => self
                .call_builtin_kw(other, &args, &kwargs)?
                .ok_or_else(|| Flow::Err(format!("unknown builtin '{other}'"))),
        }
    }

    fn to_iter(&mut self, val: Val) -> Result<Vec<Val>, Flow> {
        if is_py_handle_id(&val).is_some() {
            let built = self.py_import("builtins")?;
            let list_v = self.py_call_method(&built, "list", &[val], &[])?;
            return self.to_iter(list_v);
        }
        match val {
            Val::List(xs) => Ok(xs),
            Val::Str(s) => Ok(s.chars().map(|c| Val::Str(c.to_string())).collect()),
            Val::Map(pairs) => Ok(pairs.into_iter().map(|(k, _)| k).collect()),
            Val::Generator(id) => {
                let entry = self
                    .generators
                    .get_mut(id)
                    .ok_or_else(|| Flow::Err(format!("unknown generator {id}")))?;
                let out = entry.0[entry.1..].to_vec();
                entry.1 = entry.0.len();
                Ok(out)
            }
            _ => Err(Flow::Err("value is not iterable".into())),
        }
    }

    fn sorted_with_opts(
        &mut self,
        items: Vec<Val>,
        key_fn: Option<Val>,
        reverse: bool,
    ) -> Result<Vec<Val>, Flow> {
        let mut keyed: Vec<(Val, Val)> = Vec::with_capacity(items.len());
        for item in items {
            let k = match &key_fn {
                Some(f) => self.call_val(f, vec![item.clone()])?,
                None => item.clone(),
            };
            keyed.push((k, item));
        }
        keyed.sort_by(|(ka, _), (kb, _)| {
            let ord = match (ka, kb) {
                (Val::Num(x), Val::Num(y)) => {
                    x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal)
                }
                (Val::Str(x), Val::Str(y)) => x.cmp(y),
                _ => ka.show().cmp(&kb.show()),
            };
            if reverse { ord.reverse() } else { ord }
        });
        Ok(keyed.into_iter().map(|(_, v)| v).collect())
    }

    fn call_method(&mut self, recv: Val, name: &str, args: Vec<Val>) -> Result<Val, Flow> {
        self.call_method_kw(recv, name, args, vec![])
    }

    fn call_method_kw(
        &mut self,
        recv: Val,
        name: &str,
        args: Vec<Val>,
        kwargs: Vec<(String, Val)>,
    ) -> Result<Val, Flow> {
        if is_py_handle_id(&recv).is_some() {
            return self.py_call_method(&recv, name, &args, &kwargs);
        }
        match recv {
            Val::Object(id) => {
                let (cls_name, has_cls_tag, has_field, is_special) = {
                    let obj = self
                        .objects
                        .get(id)
                        .ok_or_else(|| Flow::Err("dangling object".into()))?;
                    (
                        obj.class_name.clone(),
                        obj.fields.contains_key("__kovrek_class__"),
                        obj.fields.contains_key(name),
                        matches!(
                            obj.class_name.as_str(),
                            "__super__" | "__property__" | "__bound_method__"
                        ),
                    )
                };
                if !has_cls_tag && !is_special && !has_field {
                    let mro = self.class_mro_slice(&cls_name);
                    if let Some((owner, false, _, Some(method_id))) =
                        self.find_member_in_mro_slice(&mro, name)
                    {
                        let mut call_args = vec![Val::Object(id)];
                        call_args.extend(args);
                        return self.call_fn_id_ctx(
                            method_id,
                            call_args,
                            kwargs,
                            Some(owner),
                            Some(Val::Object(id)),
                        );
                    }
                }
                let member = self.get_member(Val::Object(id), name)?;
                self.call_val_kw(&member, args, kwargs)
            }
            Val::Map(mut pairs) => {
                if let Some((_, member)) = pairs.iter().find(|(k, _)| *k == Val::Str(name.into())) {
                    if matches!(member, Val::Fn(_) | Val::Str(_) | Val::Object(_)) {
                        let m = member.clone();
                        return self.call_val_kw(&m, args, kwargs);
                    }
                }
                match name {
                    "keys" => Ok(Val::List(pairs.into_iter().map(|(k, _)| k).collect())),
                    "values" => Ok(Val::List(pairs.into_iter().map(|(_, v)| v).collect())),
                    "items" | "entries" => Ok(Val::List(
                        pairs.into_iter().map(|(k, v)| Val::List(vec![k, v])).collect(),
                    )),
                    "get" => {
                        if args.is_empty() || args.len() > 2 {
                            return Err(Flow::Err("map.get expects (key[, default])".into()));
                        }
                        Ok(pairs
                            .iter()
                            .find(|(k, _)| *k == args[0])
                            .map(|(_, v)| v.clone())
                            .unwrap_or_else(|| args.get(1).cloned().unwrap_or(Val::Null)))
                    }
                    "has" | "contains" => {
                        if args.len() != 1 {
                            return Err(Flow::Err("map.has expects 1 argument".into()));
                        }
                        Ok(Val::Bool(pairs.iter().any(|(k, _)| *k == args[0])))
                    }
                    "pop" => {
                        if args.is_empty() || args.len() > 2 {
                            return Err(Flow::Err("map.pop expects (key[, default])".into()));
                        }
                        if let Some(pos) = pairs.iter().position(|(k, _)| *k == args[0]) {
                            Ok(pairs.remove(pos).1)
                        } else if args.len() == 2 {
                            Ok(args[1].clone())
                        } else {
                            Err(Flow::Err(format!("key '{}' not found", args[0].show())))
                        }
                    }
                    "update" => {
                        if args.len() != 1 {
                            return Err(Flow::Err("map.update expects 1 Map argument".into()));
                        }
                        let Val::Map(other) = args.into_iter().next().unwrap() else {
                            return Err(Flow::Err("map.update expects Map".into()));
                        };
                        for (ok, ov) in other {
                            if let Some((_, existing)) = pairs.iter_mut().find(|(k, _)| *k == ok) {
                                *existing = ov;
                            } else {
                                pairs.push((ok, ov));
                            }
                        }
                        Ok(Val::Map(pairs))
                    }
                    "setdefault" => {
                        if args.is_empty() || args.len() > 2 {
                            return Err(Flow::Err("map.setdefault expects (key[, default])".into()));
                        }
                        if let Some((_, v)) = pairs.iter().find(|(k, _)| *k == args[0]) {
                            Ok(v.clone())
                        } else {
                            Ok(args.get(1).cloned().unwrap_or(Val::Null))
                        }
                    }
                    "clear" => Ok(Val::Map(vec![])),
                    "copy" => Ok(Val::Map(pairs)),
                    "len" => Ok(Val::Num(pairs.len() as f64)),
                    _ => Err(Flow::Err(format!("undefined module member '{name}'"))),
                }
            }
            Val::Str(s) => match name {
                "upper" => Ok(Val::Str(s.to_uppercase())),
                "lower" => Ok(Val::Str(s.to_lowercase())),
                "trim" | "strip" => Ok(Val::Str(s.trim().to_string())),
                "capitalize" => {
                    let mut chars = s.chars();
                    let out = match chars.next() {
                        Some(c) => {
                            format!("{}{}", c.to_uppercase(), chars.as_str().to_lowercase())
                        }
                        None => String::new(),
                    };
                    Ok(Val::Str(out))
                }
                "title" => {
                    let mut out = String::with_capacity(s.len());
                    let mut new_word = true;
                    for c in s.chars() {
                        if c.is_alphabetic() {
                            if new_word {
                                out.extend(c.to_uppercase());
                                new_word = false;
                            } else {
                                out.extend(c.to_lowercase());
                            }
                        } else {
                            out.push(c);
                            new_word = true;
                        }
                    }
                    Ok(Val::Str(out))
                }
                "zfill" => match args.first() {
                    Some(Val::Num(n)) => {
                        let w = (*n).max(0.0) as usize;
                        let char_len = s.chars().count();
                        if char_len >= w {
                            Ok(Val::Str(s))
                        } else if let Some(rest) = s.strip_prefix('-') {
                            Ok(Val::Str(format!("-{}{rest}", "0".repeat(w - char_len))))
                        } else if let Some(rest) = s.strip_prefix('+') {
                            Ok(Val::Str(format!("+{}{rest}", "0".repeat(w - char_len))))
                        } else {
                            Ok(Val::Str(format!("{}{s}", "0".repeat(w - char_len))))
                        }
                    }
                    _ => Err(Flow::Err("str.zfill expects width Number".into())),
                },
                "splitlines" => Ok(Val::List(
                    s.lines().map(|l| Val::Str(l.to_string())).collect(),
                )),
                "split" => match args.first() {
                    Some(Val::Str(sep)) => Ok(Val::List(
                        s.split(sep.as_str())
                            .map(|part| Val::Str(part.to_string()))
                            .collect(),
                    )),
                    None => Ok(Val::List(
                        s.split_whitespace()
                            .map(|part| Val::Str(part.to_string()))
                            .collect(),
                    )),
                    _ => Err(Flow::Err("str.split separator must be String".into())),
                },
                "replace" => match (args.first(), args.get(1)) {
                    (Some(Val::Str(old)), Some(Val::Str(new))) => {
                        Ok(Val::Str(s.replace(old.as_str(), new.as_str())))
                    }
                    _ => Err(Flow::Err("str.replace expects (old, new) Strings".into())),
                },
                "startswith" => match args.first() {
                    Some(Val::Str(p)) => Ok(Val::Bool(s.starts_with(p.as_str()))),
                    _ => Err(Flow::Err("str.startswith expects String".into())),
                },
                "endswith" => match args.first() {
                    Some(Val::Str(p)) => Ok(Val::Bool(s.ends_with(p.as_str()))),
                    _ => Err(Flow::Err("str.endswith expects String".into())),
                },
                "find" => match args.first() {
                    Some(Val::Str(sub)) => {
                        Ok(Val::Num(s.find(sub.as_str()).map(|i| i as f64).unwrap_or(-1.0)))
                    }
                    _ => Err(Flow::Err("str.find expects String".into())),
                },
                "rfind" => match args.first() {
                    Some(Val::Str(sub)) => {
                        Ok(Val::Num(s.rfind(sub.as_str()).map(|i| i as f64).unwrap_or(-1.0)))
                    }
                    _ => Err(Flow::Err("str.rfind expects String".into())),
                },
                "count" => match args.first() {
                    Some(Val::Str(sub)) => Ok(Val::Num(s.matches(sub.as_str()).count() as f64)),
                    _ => Err(Flow::Err("str.count expects String".into())),
                },
                "join" => {
                    let Some(iter_arg) = args.into_iter().next() else {
                        return Err(Flow::Err("str.join expects iterable".into()));
                    };
                    let items = self.to_iter(iter_arg)?;
                    let mut parts = Vec::with_capacity(items.len());
                    for item in &items {
                        parts.push(self.show_val_runtime(item)?);
                    }
                    Ok(Val::Str(parts.join(&s)))
                }
                "chars" => Ok(Val::List(s.chars().map(|c| Val::Str(c.to_string())).collect())),
                "isdigit" => Ok(Val::Bool(!s.is_empty() && s.chars().all(|c| c.is_ascii_digit()))),
                "isalpha" => Ok(Val::Bool(!s.is_empty() && s.chars().all(|c| c.is_alphabetic()))),
                "isalnum" => Ok(Val::Bool(!s.is_empty() && s.chars().all(|c| c.is_alphanumeric()))),
                "isspace" => Ok(Val::Bool(!s.is_empty() && s.chars().all(|c| c.is_whitespace()))),
                "len" => Ok(Val::Num(s.chars().count() as f64)),
                _ => Err(Flow::Err(format!("unknown String method '{name}'"))),
            },
            Val::List(mut items) => match name {
                "append" | "push" => {
                    if args.len() != 1 {
                        return Err(Flow::Err("list.append expects 1 argument".into()));
                    }
                    items.push(args[0].clone());
                    Ok(Val::List(items))
                }
                "extend" => {
                    let Some(iter_arg) = args.into_iter().next() else {
                        return Err(Flow::Err("list.extend expects iterable".into()));
                    };
                    let more = self.to_iter(iter_arg)?;
                    items.extend(more);
                    Ok(Val::List(items))
                }
                "pop" => Ok(items.pop().unwrap_or(Val::Null)),
                "clear" => Ok(Val::List(vec![])),
                "copy" => Ok(Val::List(items)),
                "insert" => match (args.first(), args.get(1)) {
                    (Some(Val::Num(i)), Some(v)) => {
                        let idx = (*i as isize).max(0).min(items.len() as isize) as usize;
                        items.insert(idx, v.clone());
                        Ok(Val::List(items))
                    }
                    _ => Err(Flow::Err("list.insert expects (index, value)".into())),
                },
                "remove" => {
                    if args.len() != 1 {
                        return Err(Flow::Err("list.remove expects 1 argument".into()));
                    }
                    if let Some(pos) = items.iter().position(|x| *x == args[0]) {
                        items.remove(pos);
                    }
                    Ok(Val::List(items))
                }
                "index" => {
                    if args.len() != 1 {
                        return Err(Flow::Err("list.index expects 1 argument".into()));
                    }
                    Ok(Val::Num(
                        items
                            .iter()
                            .position(|x| *x == args[0])
                            .map(|i| i as f64)
                            .unwrap_or(-1.0),
                    ))
                }
                "count" => {
                    if args.len() != 1 {
                        return Err(Flow::Err("list.count expects 1 argument".into()));
                    }
                    Ok(Val::Num(items.iter().filter(|x| **x == args[0]).count() as f64))
                }
                "contains" => {
                    if args.len() != 1 {
                        return Err(Flow::Err("list.contains expects 1 argument".into()));
                    }
                    Ok(Val::Bool(items.iter().any(|x| *x == args[0])))
                }
                "join" => {
                    let sep = match args.first() {
                        Some(Val::Str(s)) => s.clone(),
                        None => String::new(),
                        _ => return Err(Flow::Err("list.join separator must be String".into())),
                    };
                    let mut parts = Vec::with_capacity(items.len());
                    for item in &items {
                        parts.push(self.show_val_runtime(item)?);
                    }
                    Ok(Val::Str(parts.join(&sep)))
                }
                "reverse" => {
                    items.reverse();
                    Ok(Val::List(items))
                }
                "sort" => {
                    let mut key_fn = None;
                    let mut reverse = false;
                    for (k, v) in kwargs {
                        match k.as_str() {
                            "key" => key_fn = if v == Val::Null { None } else { Some(v) },
                            "reverse" => reverse = v.truthy(),
                            other => return Err(Flow::Err(format!("unknown list.sort argument '{other}'"))),
                        }
                    }
                    Ok(Val::List(self.sorted_with_opts(items, key_fn, reverse)?))
                }
                "len" => Ok(Val::Num(items.len() as f64)),
                _ => Err(Flow::Err(format!("unknown List method '{name}'"))),
            },
            _ => Err(Flow::Err("method call requires object or map/string/list".into())),
        }
    }

    fn call_builtin(&mut self, name: &str, args: &[Val]) -> Result<Option<Val>, Flow> {
        self.call_builtin_kw(name, args, &[])
    }

    fn call_builtin_kw(
        &mut self,
        name: &str,
        args: &[Val],
        kwargs: &[(String, Val)],
    ) -> Result<Option<Val>, Flow> {
        let kw_find = |key: &str| -> Option<Val> {
            kwargs.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.clone())
        };
        if bigint::is_bigint_builtin(name) {
            let bargs: Vec<bigint::BigArg> = args.iter().map(aot_val_to_big_arg).collect();
            let res = bigint::eval_bigint_builtin(name, &bargs).map_err(Flow::Err)?;
            return Ok(res.map(aot_big_out_to_val));
        }
        let out = match name {
            "print" | "println" => {
                let mut sep = String::from(" ");
                let mut end = String::from("\n");
                for (k, v) in kwargs {
                    match k.as_str() {
                        "sep" => sep = self.show_val_runtime(v)?,
                        "end" => end = self.show_val_runtime(v)?,
                        other => return Err(Flow::Err(format!("unknown print keyword '{other}'"))),
                    }
                }
                let mut parts = Vec::with_capacity(args.len());
                for a in args {
                    parts.push(self.show_val_runtime(a)?);
                }
                let rendered = parts.join(&sep);
                print!("{rendered}{end}");
                let _ = std::io::stdout().flush();
                Val::Null
            }
            "len" => {
                if args.len() != 1 {
                    return Err(Flow::Err("len expects 1 argument".into()));
                }
                if is_py_handle_id(&args[0]).is_some() {
                    self.py_call_method(&args[0], "__len__", &[], &[])?
                } else if let Some(res) = self.try_dunder(&args[0], "__len__", vec![], vec![])? {
                    res
                } else {
                    match &args[0] {
                        Val::Str(s) => Val::Num(s.chars().count() as f64),
                        Val::List(v) => Val::Num(v.len() as f64),
                        Val::Map(v) => Val::Num(v.len() as f64),
                        _ => return Err(Flow::Err("len supports String/List/Map".into())),
                    }
                }
            }
            "range" => {
                let (start, end, step) = match args {
                    [Val::Num(e)] => (0i64, *e as i64, 1i64),
                    [Val::Num(s), Val::Num(e)] => (*s as i64, *e as i64, 1i64),
                    [Val::Num(s), Val::Num(e), Val::Num(st)] => (*s as i64, *e as i64, *st as i64),
                    _ => return Err(Flow::Err("range expects 1..3 numbers".into())),
                };
                if step == 0 {
                    return Err(Flow::Err("range step cannot be 0".into()));
                }
                let mut out = Vec::new();
                let mut i = start;
                while (step > 0 && i < end) || (step < 0 && i > end) {
                    out.push(Val::Num(i as f64));
                    i += step;
                }
                Val::List(out)
            }
            "__iter_list" | "collect" => {
                if args.len() != 1 {
                    return Err(Flow::Err(format!("{name} expects 1 argument")));
                }
                Val::List(self.to_iter(args[0].clone())?)
            }
            "next" => {
                if args.is_empty() || args.len() > 2 {
                    return Err(Flow::Err("next expects (generator[, default])".into()));
                }
                let Val::Generator(id) = args[0] else {
                    return Err(Flow::Err("next expects Generator".into()));
                };
                let entry = self
                    .generators
                    .get_mut(id)
                    .ok_or_else(|| Flow::Err(format!("unknown generator {id}")))?;
                if entry.1 < entry.0.len() {
                    let v = entry.0[entry.1].clone();
                    entry.1 += 1;
                    v
                } else if let Some(def) = args.get(1) {
                    def.clone()
                } else {
                    return Err(Flow::Err("generator exhausted".into()));
                }
            }
            "type_of" => {
                if args.len() != 1 {
                    return Err(Flow::Err("type_of expects 1 argument".into()));
                }
                match &args[0] {
                    Val::Num(n) => Val::Str(if n.fract() == 0.0 { "Int" } else { "Number" }.into()),
                    Val::Str(_) => Val::Str("String".into()),
                    Val::Bool(_) => Val::Str("Bool".into()),
                    Val::Null => Val::Str("Null".into()),
                    Val::List(_) => Val::Str("List".into()),
                    Val::Map(_) => Val::Str("Map".into()),
                    Val::Fn(_) => Val::Str("Function".into()),
                    Val::Generator(_) => Val::Str("Generator".into()),
                    Val::Object(id) => Val::Str(
                        self.objects
                            .get(*id)
                            .map(|o| o.class_name.clone())
                            .unwrap_or_else(|| "Object".into()),
                    ),
                }
            }
            "assert" => {
                if args.is_empty() || args.len() > 2 {
                    return Err(Flow::Err("assert expects 1 or 2 arguments".into()));
                }
                if !args[0].truthy() {
                    let msg = match args.get(1) {
                        Some(m) => self.show_val_runtime(m)?,
                        None => "assertion failed".into(),
                    };
                    return Err(Flow::Err(msg));
                }
                Val::Null
            }
            "requires" | "ensures" => {
                if args.is_empty() || args.len() > 2 {
                    return Err(Flow::Err(format!("{name} expects (cond[, message])")));
                }
                if !args[0].truthy() {
                    let kind = if name == "requires" {
                        "precondition"
                    } else {
                        "postcondition"
                    };
                    let msg = match args.get(1) {
                        Some(m) => self.show_val_runtime(m)?,
                        None => format!("{kind} failed"),
                    };
                    return Err(Flow::Err(format!("ContractError: {msg}")));
                }
                Val::Null
            }
            "str" => {
                if args.len() != 1 {
                    return Err(Flow::Err("str expects 1 argument".into()));
                }
                Val::Str(self.show_val_runtime(&args[0])?)
            }
            "repr" => {
                if args.len() != 1 {
                    return Err(Flow::Err("repr expects 1 argument".into()));
                }
                Val::Str(self.repr_val_runtime(&args[0])?)
            }
            "callable" => {
                if args.len() != 1 {
                    return Err(Flow::Err("callable expects 1 argument".into()));
                }
                let ok = match &args[0] {
                    Val::Fn(_) => true,
                    Val::Object(id) => self.objects.get(*id).is_some_and(|o| {
                        o.fields.contains_key("__kovrek_class__")
                            || o.class_name == "__bound_method__"
                            || (!matches!(
                                o.class_name.as_str(),
                                "__property__" | "__staticmethod__" | "__classmethod__" | "__super__"
                            ) && self.find_method(&o.class_name, "__call__").is_some())
                    }),
                    Val::Map(_) if is_py_handle_id(&args[0]).is_some() => {
                        let built = self.py_import("builtins")?;
                        self.py_call_method(&built, "callable", std::slice::from_ref(&args[0]), &[])?
                            .truthy()
                    }
                    _ => false,
                };
                Val::Bool(ok)
            }
            "divmod" => {
                if args.len() != 2 {
                    return Err(Flow::Err("divmod expects 2 arguments".into()));
                }
                let (Val::Num(x), Val::Num(y)) = (&args[0], &args[1]) else {
                    return Err(Flow::Err("divmod expects Numbers".into()));
                };
                if *y == 0.0 {
                    return Err(Flow::Err("division by zero in divmod".into()));
                }
                let q = (x / y).floor();
                let r = x - y * q;
                Val::List(vec![Val::Num(q), Val::Num(r)])
            }
            "getattr" => {
                if args.len() < 2 || args.len() > 3 {
                    return Err(Flow::Err("getattr expects (obj, name[, default])".into()));
                }
                let Val::Str(attr) = &args[1] else {
                    return Err(Flow::Err("getattr attribute name must be String".into()));
                };
                match self.get_member(args[0].clone(), attr) {
                    Ok(v) => v,
                    Err(_) if args.len() == 3 => args[2].clone(),
                    Err(e) => return Err(e),
                }
            }
            "hasattr" => {
                if args.len() != 2 {
                    return Err(Flow::Err("hasattr expects (obj, name)".into()));
                }
                let Val::Str(attr) = &args[1] else {
                    return Err(Flow::Err("hasattr attribute name must be String".into()));
                };
                let ok = if is_py_handle_id(&args[0]).is_some() {
                    self.py_hasattr(&args[0], attr)?
                } else {
                    self.get_member(args[0].clone(), attr).is_ok()
                };
                Val::Bool(ok)
            }
            "setattr" => {
                if args.len() != 3 {
                    return Err(Flow::Err("setattr expects (obj, name, value)".into()));
                }
                let Val::Str(attr) = &args[1] else {
                    return Err(Flow::Err("setattr attribute name must be String".into()));
                };
                self.set_member(args[0].clone(), attr, args[2].clone())?;
                Val::Null
            }
            "delattr" => {
                if args.len() != 2 {
                    return Err(Flow::Err("delattr expects (obj, name)".into()));
                }
                let Val::Str(attr) = &args[1] else {
                    return Err(Flow::Err("delattr attribute name must be String".into()));
                };
                self.del_member(args[0].clone(), attr)?;
                Val::Null
            }
            "type" => {
                if args.len() == 1 {
                    let cls_name = match &args[0] {
                        Val::Num(n) => {
                            if n.fract() == 0.0 {
                                "int".to_string()
                            } else {
                                "float".to_string()
                            }
                        }
                        Val::Str(_) => "str".to_string(),
                        Val::Bool(_) => "bool".to_string(),
                        Val::Null => "null".to_string(),
                        Val::List(_) => "list".to_string(),
                        Val::Map(_) => "dict".to_string(),
                        Val::Fn(_) => "function".to_string(),
                        Val::Generator(_) => "object".to_string(),
                        Val::Object(id) => self
                            .objects
                            .get(*id)
                            .map(|o| o.class_name.clone())
                            .unwrap_or_else(|| "object".into()),
                    };
                    self.class_object_val(&cls_name)?
                } else if args.len() == 3 {
                    let Val::Str(ref cls_name) = args[0] else {
                        return Err(Flow::Err(
                            "type(name, bases, namespace) name must be String".into(),
                        ));
                    };
                    let base_items = self.to_iter(args[1].clone())?;
                    let mut bases = Vec::with_capacity(base_items.len());
                    for b in &base_items {
                        bases.push(self.resolve_class_name_arg(b)?);
                    }
                    let Val::Map(ref pairs) = args[2] else {
                        return Err(Flow::Err(
                            "type(name, bases, namespace) namespace must be a Map".into(),
                        ));
                    };
                    self.register_class_from_ns("type", cls_name, &bases, pairs.clone())?
                } else {
                    return Err(Flow::Err(
                        "type expects 1 argument (obj) or 3 arguments (name, bases, namespace)"
                            .into(),
                    ));
                }
            }
            "super" => self.make_super_obj(args)?,
            "property" => {
                let mut fget = args.first().cloned().unwrap_or(Val::Null);
                let mut fset = args.get(1).cloned().unwrap_or(Val::Null);
                let mut fdel = args.get(2).cloned().unwrap_or(Val::Null);
                if args.len() > 3 {
                    return Err(Flow::Err(
                        "property expects at most 3 positional arguments".into(),
                    ));
                }
                let mut kw = kwargs.to_vec();
                if let Some(v) = Self::pop_kw(&mut kw, "fget") {
                    fget = v;
                }
                if let Some(v) = Self::pop_kw(&mut kw, "fset") {
                    fset = v;
                }
                if let Some(v) = Self::pop_kw(&mut kw, "fdel") {
                    fdel = v;
                }
                if let Some((bad, _)) = kw.first() {
                    return Err(Flow::Err(format!(
                        "property got unexpected keyword argument '{bad}'"
                    )));
                }
                self.make_property_obj(fget, fset, fdel)
            }
            "staticmethod" => {
                if args.len() != 1 {
                    return Err(Flow::Err("staticmethod expects 1 callable argument".into()));
                }
                self.make_descriptor_wrapper("__staticmethod__", args[0].clone())
            }
            "classmethod" => {
                if args.len() != 1 {
                    return Err(Flow::Err("classmethod expects 1 callable argument".into()));
                }
                self.make_descriptor_wrapper("__classmethod__", args[0].clone())
            }
            "mro" => {
                if args.len() != 1 {
                    return Err(Flow::Err("mro expects 1 class argument".into()));
                }
                let cls_name = self.resolve_class_name_arg(&args[0])?;
                let mro = self.class_mro_slice(&cls_name);
                let mut out = Vec::with_capacity(mro.len());
                for c in &mro {
                    out.push(self.class_object_val(c)?);
                }
                Val::List(out)
            }
            "issubclass" => {
                if args.len() != 2 {
                    return Err(Flow::Err("issubclass expects (cls, class_or_tuple)".into()));
                }
                let sub = self.resolve_class_name_arg(&args[0])?;
                let ok = match &args[1] {
                    Val::List(items) => {
                        let mut matched = false;
                        for item in items {
                            let sup = self.resolve_class_name_arg(item)?;
                            if self.class_is_a(&sub, &sup) {
                                matched = true;
                                break;
                            }
                        }
                        matched
                    }
                    other => {
                        let sup = self.resolve_class_name_arg(other)?;
                        self.class_is_a(&sub, &sup)
                    }
                };
                Val::Bool(ok)
            }
            "int" => match args.first() {
                Some(Val::Num(n)) => Val::Num(n.trunc()),
                Some(Val::Bool(b)) => Val::Num(if *b { 1.0 } else { 0.0 }),
                Some(Val::Str(s)) => Val::Num(
                    s.trim()
                        .parse::<f64>()
                        .map(|n| n.trunc())
                        .map_err(|_| Flow::Err(format!("invalid int '{s}'")))?,
                ),
                _ => return Err(Flow::Err("int expects Number/Bool/String".into())),
            },
            "float" => match args.first() {
                Some(Val::Num(n)) => Val::Num(*n),
                Some(Val::Bool(b)) => Val::Num(if *b { 1.0 } else { 0.0 }),
                Some(Val::Str(s)) => Val::Num(
                    s.trim()
                        .parse::<f64>()
                        .map_err(|_| Flow::Err(format!("invalid float '{s}'")))?,
                ),
                _ => return Err(Flow::Err("float expects Number/Bool/String".into())),
            },
            "bool" => {
                if args.len() != 1 {
                    return Err(Flow::Err("bool expects 1 argument".into()));
                }
                Val::Bool(args[0].truthy())
            }
            "round" => {
                if args.is_empty() || args.len() > 2 {
                    return Err(Flow::Err("round expects number[, ndigits]".into()));
                }
                let Val::Num(n) = args[0] else {
                    return Err(Flow::Err("round expects Number".into()));
                };
                let ndigits = args.get(1).cloned().or_else(|| kw_find("ndigits"));
                match ndigits {
                    None | Some(Val::Null) => Val::Num(n.round()),
                    Some(Val::Num(d)) => {
                        let nd = d as i32;
                        if nd <= 0 {
                            let scale = 10f64.powi(-nd);
                            Val::Num((n / scale).round() * scale)
                        } else {
                            let scale = 10f64.powi(nd);
                            Val::Num((n * scale).round() / scale)
                        }
                    }
                    _ => return Err(Flow::Err("round ndigits must be Number".into())),
                }
            }
            "abs" | "sqrt" | "floor" | "ceil" | "sin" | "cos" | "tan" | "log" | "exp" => {
                if args.len() != 1 {
                    return Err(Flow::Err(format!("{name} expects 1 number")));
                }
                let Val::Num(x) = args[0] else {
                    return Err(Flow::Err(format!("{name} expects number")));
                };
                let r = match name {
                    "abs" => x.abs(),
                    "sqrt" => x.sqrt(),
                    "floor" => x.floor(),
                    "ceil" => x.ceil(),
                    "sin" => x.sin(),
                    "cos" => x.cos(),
                    "tan" => x.tan(),
                    "log" => x.ln(),
                    _ => x.exp(),
                };
                Val::Num(r)
            }
            "pow" => match (args.first(), args.get(1), args.get(2)) {
                (Some(Val::Num(a)), Some(Val::Num(b)), None) => Val::Num(a.powf(*b)),
                (Some(Val::Num(a)), Some(Val::Num(b)), Some(Val::Num(m))) => {
                    if *m == 0.0 {
                        return Err(Flow::Err("pow modulus cannot be zero".into()));
                    }
                    Val::Num(a.powf(*b) % *m)
                }
                _ => return Err(Flow::Err("pow expects base,exponent[,modulus]".into())),
            },
            "clamp" => match args {
                [Val::Num(x), Val::Num(lo), Val::Num(hi)] => Val::Num(x.clamp(*lo, *hi)),
                _ => return Err(Flow::Err("clamp expects 3 numbers".into())),
            },
            "sum" => {
                if args.is_empty() || args.len() > 2 {
                    return Err(Flow::Err("sum expects iterable[, start]".into()));
                }
                let mut total = match args.get(1).cloned().or_else(|| kw_find("start")) {
                    Some(Val::Num(n)) => n,
                    None | Some(Val::Null) => 0.0,
                    _ => return Err(Flow::Err("sum start must be Number".into())),
                };
                let xs = self.to_iter(args[0].clone())?;
                for x in xs {
                    match x {
                        Val::Num(n) => total += n,
                        _ => return Err(Flow::Err("sum expects numbers".into())),
                    }
                }
                Val::Num(total)
            }
            "min" | "max" => {
                let items = if args.len() == 1 {
                    self.to_iter(args[0].clone())?
                } else if !args.is_empty() {
                    args.to_vec()
                } else {
                    return Err(Flow::Err(format!("{name} expects iterable or args")));
                };
                if items.is_empty() {
                    if let Some(def) = kw_find("default") {
                        return Ok(Some(def));
                    }
                    return Err(Flow::Err(format!("{name} empty sequence")));
                }
                let key_fn = kw_find("key").filter(|v| *v != Val::Null);
                let sorted = self.sorted_with_opts(items, key_fn, false)?;
                if name == "min" {
                    sorted.into_iter().next().unwrap()
                } else {
                    sorted.into_iter().last().unwrap()
                }
            }
            "all" => {
                if args.len() != 1 {
                    return Err(Flow::Err("all expects 1 iterable".into()));
                }
                let xs = self.to_iter(args[0].clone())?;
                Val::Bool(xs.iter().all(Val::truthy))
            }
            "any" => {
                if args.len() != 1 {
                    return Err(Flow::Err("any expects 1 iterable".into()));
                }
                let xs = self.to_iter(args[0].clone())?;
                Val::Bool(xs.iter().any(Val::truthy))
            }
            "append" | "push" => match args {
                [Val::List(xs), val] => {
                    let mut out = xs.clone();
                    out.push(val.clone());
                    Val::List(out)
                }
                _ => return Err(Flow::Err("append expects (List, Val)".into())),
            },
            "pop" => match args.first() {
                Some(Val::List(xs)) => xs.last().cloned().unwrap_or(Val::Null),
                _ => return Err(Flow::Err("pop expects List".into())),
            },
            "slice" => match args {
                [Val::List(xs), Val::Num(s), Val::Num(e)] => {
                    let lo = (*s as isize).max(0).min(xs.len() as isize) as usize;
                    let hi = (*e as isize).max(lo as isize).min(xs.len() as isize) as usize;
                    Val::List(xs[lo..hi].to_vec())
                }
                [Val::Str(s), Val::Num(a), Val::Num(b)] => {
                    let cs: Vec<char> = s.chars().collect();
                    let lo = (*a as isize).max(0).min(cs.len() as isize) as usize;
                    let hi = (*b as isize).max(lo as isize).min(cs.len() as isize) as usize;
                    Val::Str(cs[lo..hi].iter().collect())
                }
                _ => return Err(Flow::Err("slice expects (List|Str, start, end)".into())),
            },
            "reverse" => match args.first() {
                Some(v) => {
                    let mut out = self.to_iter(v.clone())?;
                    out.reverse();
                    Val::List(out)
                }
                _ => return Err(Flow::Err("reverse expects iterable".into())),
            },
            "sort" | "sorted" => {
                if args.len() != 1 {
                    return Err(Flow::Err(format!("{name} expects 1 iterable")));
                }
                let mut key_fn = None;
                let mut reverse = false;
                for (k, v) in kwargs {
                    match k.as_str() {
                        "key" => key_fn = if *v == Val::Null { None } else { Some(v.clone()) },
                        "reverse" => reverse = v.truthy(),
                        other => return Err(Flow::Err(format!("unknown {name} keyword '{other}'"))),
                    }
                }
                let items = self.to_iter(args[0].clone())?;
                Val::List(self.sorted_with_opts(items, key_fn, reverse)?)
            }
            "unique" => match args.first() {
                Some(v) => {
                    let xs = self.to_iter(v.clone())?;
                    let mut out = Vec::new();
                    for x in xs {
                        if !out.contains(&x) {
                            out.push(x);
                        }
                    }
                    Val::List(out)
                }
                _ => return Err(Flow::Err("unique expects iterable".into())),
            },
            "enumerate" => {
                if args.is_empty() || args.len() > 2 {
                    return Err(Flow::Err("enumerate expects (iterable[, start])".into()));
                }
                let start = match args.get(1).cloned().or_else(|| kw_find("start")) {
                    Some(Val::Num(n)) => n as i64,
                    None | Some(Val::Null) => 0,
                    _ => return Err(Flow::Err("enumerate start must be Number".into())),
                };
                let xs = self.to_iter(args[0].clone())?;
                Val::List(
                    xs.into_iter()
                        .enumerate()
                        .map(|(i, v)| Val::List(vec![Val::Num((start + i as i64) as f64), v]))
                        .collect(),
                )
            }
            "zip" => {
                if args.is_empty() {
                    Val::List(vec![])
                } else {
                    let mut cols = Vec::with_capacity(args.len());
                    for item in args {
                        cols.push(self.to_iter(item.clone())?);
                    }
                    let min_len = cols.iter().map(Vec::len).min().unwrap_or(0);
                    let mut out = Vec::with_capacity(min_len);
                    for i in 0..min_len {
                        out.push(Val::List(cols.iter().map(|c| c[i].clone()).collect()));
                    }
                    Val::List(out)
                }
            }
            "map_call" => match args {
                [fval, seq] => {
                    let xs = self.to_iter(seq.clone())?;
                    let mut out = Vec::with_capacity(xs.len());
                    for x in xs {
                        out.push(self.call_val(fval, vec![x])?);
                    }
                    Val::List(out)
                }
                _ => return Err(Flow::Err("map_call expects (callable, iterable)".into())),
            },
            "filter_call" => match args {
                [fval, seq] => {
                    let xs = self.to_iter(seq.clone())?;
                    let mut out = Vec::new();
                    for x in xs {
                        if self.call_val(fval, vec![x.clone()])?.truthy() {
                            out.push(x);
                        }
                    }
                    Val::List(out)
                }
                _ => return Err(Flow::Err("filter_call expects (callable, iterable)".into())),
            },
            "reduce_call" => match args {
                [fval, seq, init] => {
                    let xs = self.to_iter(seq.clone())?;
                    let mut acc = init.clone();
                    for x in xs {
                        acc = self.call_val(fval, vec![acc, x])?;
                    }
                    acc
                }
                _ => return Err(Flow::Err("reduce_call expects (callable, iterable, init)".into())),
            },
            "list" | "tuple" => match args.first() {
                None => Val::List(vec![]),
                Some(v) => Val::List(self.to_iter(v.clone())?),
            },
            "set" => match args.first() {
                None => Val::List(vec![]),
                Some(v) => {
                    let xs = self.to_iter(v.clone())?;
                    let mut out = Vec::new();
                    for x in xs {
                        if !out.contains(&x) {
                            out.push(x);
                        }
                    }
                    Val::List(out)
                }
            },
            "dict" => {
                if args.is_empty() {
                    Val::Map(vec![])
                } else if args.len() == 1 {
                    match &args[0] {
                        Val::Map(pairs) => Val::Map(pairs.clone()),
                        other => {
                            let items = self.to_iter(other.clone())?;
                            let mut pairs: Vec<(Val, Val)> = Vec::with_capacity(items.len());
                            for item in items {
                                match item {
                                    Val::List(kv) if kv.len() == 2 => {
                                        if let Some((_, ex)) =
                                            pairs.iter_mut().find(|(ek, _)| *ek == kv[0])
                                        {
                                            *ex = kv[1].clone();
                                        } else {
                                            pairs.push((kv[0].clone(), kv[1].clone()));
                                        }
                                    }
                                    _ => {
                                        return Err(Flow::Err(
                                            "dict expects Map or iterable of [key, value] pairs"
                                                .into(),
                                        ))
                                    }
                                }
                            }
                            Val::Map(pairs)
                        }
                    }
                } else {
                    return Err(Flow::Err("dict expects at most 1 argument".into()));
                }
            }
            "contains" => match args {
                [Val::List(xs), target] => Val::Bool(xs.contains(target)),
                [Val::Str(s), Val::Str(sub)] => Val::Bool(s.contains(sub.as_str())),
                [Val::Map(pairs), key] => Val::Bool(pairs.iter().any(|(k, _)| k == key)),
                _ => return Err(Flow::Err("contains expects container and item".into())),
            },
            "index_of" => match args {
                [Val::List(xs), target] => Val::Num(
                    xs.iter()
                        .position(|x| x == target)
                        .map(|i| i as f64)
                        .unwrap_or(-1.0),
                ),
                _ => return Err(Flow::Err("index_of expects (List, item)".into())),
            },
            "keys" => match args.first() {
                Some(Val::Map(pairs)) => Val::List(pairs.iter().map(|(k, _)| k.clone()).collect()),
                _ => return Err(Flow::Err("keys expects Map".into())),
            },
            "values" => match args.first() {
                Some(Val::Map(pairs)) => Val::List(pairs.iter().map(|(_, v)| v.clone()).collect()),
                _ => return Err(Flow::Err("values expects Map".into())),
            },
            "is_instance" | "isinstance" => {
                if args.len() != 2 {
                    return Err(Flow::Err("isinstance expects (value, class_or_type)".into()));
                }
                let matched = match &args[1] {
                    Val::Str(s) => self.value_matches_rt(&args[0], s),
                    Val::List(items) => {
                        let mut ok = false;
                        for item in items {
                            let target = match item {
                                Val::Str(s) => s.clone(),
                                _ => self.resolve_class_name_arg(item)?,
                            };
                            if self.value_matches_rt(&args[0], &target) {
                                ok = true;
                                break;
                            }
                        }
                        ok
                    }
                    other => {
                        let target = self.resolve_class_name_arg(other)?;
                        self.value_matches_rt(&args[0], &target)
                    }
                };
                Val::Bool(matched)
            }
            "has_field" => match args {
                [Val::Object(id), Val::Str(f)] => Val::Bool(
                    self.objects
                        .get(*id)
                        .map(|o| o.fields.contains_key(f))
                        .unwrap_or(false),
                ),
                _ => return Err(Flow::Err("has_field expects (Object, String)".into())),
            },
            "get_field" => match args {
                [Val::Object(id), Val::Str(f)] => self
                    .objects
                    .get(*id)
                    .and_then(|o| o.fields.get(f))
                    .cloned()
                    .unwrap_or(Val::Null),
                _ => return Err(Flow::Err("get_field expects (Object, String)".into())),
            },
            "set_field" => match args {
                [Val::Object(id), Val::Str(f), val] => {
                    self.set_member(Val::Object(*id), f, val.clone())?;
                    Val::Null
                }
                _ => return Err(Flow::Err("set_field expects (Object, String, Val)".into())),
            },
            "spawn" => {
                if args.len() != 2 {
                    return Err(Flow::Err("spawn expects callable,args".into()));
                }
                let Val::List(call_args) = args[1].clone() else {
                    return Err(Flow::Err("spawn args must be List".into()));
                };
                let task_res = match self.call_val(&args[0], call_args) {
                    Ok(v) => Ok(v),
                    Err(Flow::Err(msg)) => Err(msg),
                    Err(_) => Err("unexpected control flow in spawned task".into()),
                };
                self.store_task(task_res)
            }
            "task_done" => {
                if args.len() != 1 {
                    return Err(Flow::Err("task_done expects task id".into()));
                }
                let Val::Num(n) = args[0] else {
                    return Err(Flow::Err("task id must be Number".into()));
                };
                let id = n as u64;
                if !self.tasks.contains_key(&id) {
                    return Err(Flow::Err(format!("unknown task {id}")));
                }
                Val::Bool(true)
            }
            "task_status" => {
                if args.len() != 1 {
                    return Err(Flow::Err("task_status expects task id".into()));
                }
                let Val::Num(n) = args[0] else {
                    return Err(Flow::Err("task id must be Number".into()));
                };
                let id = n as u64;
                if !self.tasks.contains_key(&id) {
                    return Err(Flow::Err(format!("unknown task {id}")));
                }
                Val::Str("finished".into())
            }
            "await_task" => {
                if args.len() != 1 {
                    return Err(Flow::Err("await_task expects task id".into()));
                }
                self.await_val(args[0].clone())?
            }
            "await_timeout" => {
                if args.len() != 2 {
                    return Err(Flow::Err("await_timeout expects (task_id, timeout_ms)".into()));
                }
                self.await_val(args[0].clone())?
            }
            "gather" => {
                if args.len() != 1 {
                    return Err(Flow::Err("gather expects list of task ids".into()));
                }
                let tids = self.to_iter(args[0].clone())?;
                let mut out = Vec::with_capacity(tids.len());
                for tid in tids {
                    out.push(self.await_val(tid)?);
                }
                Val::List(out)
            }
            "channel" => {
                if !args.is_empty() {
                    return Err(Flow::Err("channel expects no arguments".into()));
                }
                let id = self.next_channel_id;
                self.next_channel_id += 1;
                self.channels.insert(id, VecDeque::new());
                Val::Num(id as f64)
            }
            "send" => {
                if args.len() != 2 {
                    return Err(Flow::Err("send expects channel,value".into()));
                }
                let Val::Num(n) = args[0] else {
                    return Err(Flow::Err("channel id must be Number".into()));
                };
                let ch = self
                    .channels
                    .get_mut(&(n as u64))
                    .ok_or_else(|| Flow::Err("unknown channel".into()))?;
                ch.push_back(args[1].clone());
                Val::Null
            }
            "recv" => {
                if args.len() != 1 {
                    return Err(Flow::Err("recv expects channel".into()));
                }
                let Val::Num(n) = args[0] else {
                    return Err(Flow::Err("channel id must be Number".into()));
                };
                let ch = self
                    .channels
                    .get_mut(&(n as u64))
                    .ok_or_else(|| Flow::Err("unknown channel".into()))?;
                ch.pop_front()
                    .ok_or_else(|| Flow::Err("channel empty".into()))?
            }
            "try_recv" => {
                if args.len() != 1 {
                    return Err(Flow::Err("try_recv expects channel".into()));
                }
                let Val::Num(n) = args[0] else {
                    return Err(Flow::Err("channel id must be Number".into()));
                };
                let ch = self
                    .channels
                    .get_mut(&(n as u64))
                    .ok_or_else(|| Flow::Err("unknown channel".into()))?;
                ch.pop_front().unwrap_or(Val::Null)
            }
            "py_available" => {
                if !args.is_empty() {
                    return Err(Flow::Err("py_available expects 0 arguments".into()));
                }
                Val::Bool(PyWorker::detect().is_some())
            }
            "py_import" => {
                if args.len() != 1 {
                    return Err(Flow::Err("py_import expects module name String".into()));
                }
                let Val::Str(mname) = &args[0] else {
                    return Err(Flow::Err("py_import module name must be String".into()));
                };
                self.py_import(mname)?
            }
            "py_call" => {
                if args.is_empty() || args.len() > 3 {
                    return Err(Flow::Err("py_call expects (callable[, args[, kwargs]])".into()));
                }
                let pos_args = match args.get(1) {
                    None | Some(Val::Null) => Vec::new(),
                    Some(Val::List(xs)) => xs.clone(),
                    _ => return Err(Flow::Err("py_call args must be List".into())),
                };
                let mut merged_kw = Vec::new();
                if let Some(Val::Map(pairs)) = args.get(2) {
                    self.extend_kwargs(Val::Map(pairs.clone()), &mut merged_kw)?;
                } else if args.get(2).is_some() && args.get(2) != Some(&Val::Null) {
                    return Err(Flow::Err("py_call kwargs must be Map".into()));
                }
                merged_kw.extend(kwargs.iter().cloned());
                self.py_call(&args[0], &pos_args, &merged_kw)?
            }
            "py_get" => {
                if args.len() != 2 {
                    return Err(Flow::Err("py_get expects (py_obj, attr)".into()));
                }
                let Val::Str(attr) = &args[1] else {
                    return Err(Flow::Err("py_get attr must be String".into()));
                };
                self.py_get(&args[0], attr)?
            }
            "py_set" => {
                if args.len() != 3 {
                    return Err(Flow::Err("py_set expects (py_obj, attr, value)".into()));
                }
                let Val::Str(attr) = &args[1] else {
                    return Err(Flow::Err("py_set attr must be String".into()));
                };
                self.py_set(&args[0], attr, &args[2])?
            }
            "py_dir" => {
                if args.len() != 1 {
                    return Err(Flow::Err("py_dir expects py_obj".into()));
                }
                self.py_dir(&args[0])?
            }
            "py_eval" => {
                if args.is_empty() || args.len() > 2 {
                    return Err(Flow::Err("py_eval expects (expr[, locals])".into()));
                }
                let Val::Str(expr) = &args[0] else {
                    return Err(Flow::Err("py_eval expr must be String".into()));
                };
                self.py_eval(expr, args.get(1))?
            }
            "py_exec" => {
                if args.len() != 1 {
                    return Err(Flow::Err("py_exec expects Python source String".into()));
                }
                let Val::Str(code) = &args[0] else {
                    return Err(Flow::Err("py_exec source must be String".into()));
                };
                self.py_exec(code)?;
                Val::Null
            }
            "py_info" => {
                if !args.is_empty() {
                    return Err(Flow::Err("py_info expects no args".into()));
                }
                Val::Map(vec![
                    (Val::Str("available".into()), Val::Bool(PyWorker::detect().is_some())),
                    (Val::Str("mode".into()), Val::Str("subprocess-jsonrpc".into())),
                    (Val::Str("zero_copy_buffer".into()), Val::Bool(false)),
                ])
            }
            "py_free" => {
                if args.len() != 1 {
                    return Err(Flow::Err("py_free expects Python handle".into()));
                }
                Val::Bool(self.py_free(&args[0])?)
            }
            "py_to_tensor" | "tensor_to_py" => {
                return Err(Flow::Err(
                    "py_to_tensor/tensor_to_py requires VM Tensor runtime".into(),
                ));
            }
            "clock" => Val::Num(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0),
            ),
            "sleep_ms" => match args.first() {
                Some(Val::Num(ms)) => {
                    std::thread::sleep(std::time::Duration::from_millis((*ms).max(0.0) as u64));
                    Val::Null
                }
                _ => return Err(Flow::Err("sleep_ms expects number".into())),
            },
            "exit" => {
                let code = match args.first() {
                    Some(Val::Num(n)) => *n as i32,
                    _ => 0,
                };
                std::process::exit(code);
            }
            "read_file" => match args.first() {
                Some(Val::Str(p)) => Val::Str(
                    std::fs::read_to_string(p).map_err(|e| Flow::Err(e.to_string()))?,
                ),
                _ => return Err(Flow::Err("read_file expects String path".into())),
            },
            "write_file" => match args {
                [Val::Str(p), content] => {
                    std::fs::write(p, self.show_val_runtime(content)?)
                        .map_err(|e| Flow::Err(e.to_string()))?;
                    Val::Null
                }
                _ => return Err(Flow::Err("write_file expects (String path, content)".into())),
            },
            "append_file" => match args {
                [Val::Str(p), content] => {
                    let mut f = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(p)
                        .map_err(|e| Flow::Err(e.to_string()))?;
                    f.write_all(self.show_val_runtime(content)?.as_bytes())
                        .map_err(|e| Flow::Err(e.to_string()))?;
                    Val::Null
                }
                _ => return Err(Flow::Err("append_file expects (String path, content)".into())),
            },
            "file_exists" => match args.first() {
                Some(Val::Str(p)) => Val::Bool(std::path::Path::new(p).exists()),
                _ => return Err(Flow::Err("file_exists expects String path".into())),
            },
            "env_get" => match args.first() {
                Some(Val::Str(k)) => match std::env::var(k) {
                    Ok(v) => Val::Str(v),
                    Err(_) => Val::Null,
                },
                _ => return Err(Flow::Err("env_get expects String key".into())),
            },
            "starts_with" | "ends_with" => match args {
                [Val::Str(text), Val::Str(pat)] => Val::Bool(if name == "starts_with" {
                    text.starts_with(pat)
                } else {
                    text.ends_with(pat)
                }),
                _ => return Err(Flow::Err(format!("{name} expects (String, String)"))),
            },
            "jit_available" => Val::Bool(false),
            "jit_set_enabled" => Val::Bool(false),
            "jit_compile" => Val::Bool(true),
            "jit_info" | "jit_stats" => Val::Map(vec![
                (Val::Str("available".into()), Val::Bool(false)),
                (Val::Str("enabled".into()), Val::Bool(false)),
                (Val::Str("backend".into()), Val::Str("aot-rustc-native".into())),
                (Val::Str("arch".into()), Val::Str(std::env::consts::ARCH.into())),
                (Val::Str("os".into()), Val::Str(std::env::consts::OS.into())),
            ]),
            "gpu_available" => Val::Bool(false),
            "input" => {
                if args.len() > 1 {
                    return Err(Flow::Err("input expects at most 1 argument (prompt)".into()));
                }
                if let Some(arg) = args.first() {
                    if !matches!(arg, Val::Null) {
                        let prompt = self.show_val_runtime(arg)?;
                        print!("{prompt}");
                        std::io::stdout()
                            .flush()
                            .map_err(|e| Flow::Err(e.to_string()))?;
                    }
                }
                let mut line = String::new();
                let n = std::io::stdin()
                    .read_line(&mut line)
                    .map_err(|e| Flow::Err(e.to_string()))?;
                if n == 0 {
                    Val::Null
                } else {
                    while line.ends_with('\n') || line.ends_with('\r') {
                        line.pop();
                    }
                    Val::Str(line)
                }
            }
            _ => return Ok(None),
        };
        Ok(Some(out))
    }
}

"####;
