// Compile pipeline: source to bytecode, plus the convenience entry points
// used by the CLI, the LSP and the tests.
//
// Extracted from `main.rs` without behavior change; see docs/ENGINE-SPLIT.md.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value as JsonValue;

use crate::ast::{CallArg, Expr, Stmt};
use crate::bytecode::{Op, decode_bytecode, encode_bytecode};
use crate::compiler::{CompiledDebug, Compiler};
use crate::dap::DebugMap;
use crate::ir::optimize_bytecode;
use crate::lexer::lex_with_lines;
use crate::parser::Parser;
use crate::project::project_root;
use crate::vm::Vm;

pub(crate) fn load_source_with_lines(
    path: &str,
) -> Result<(String, Vec<(PathBuf, usize)>), String> {
    fn is_valid_ident(s: &str) -> bool {
        !s.is_empty()
            && s.chars().enumerate().all(|(i, c)| {
                c == '_' || c.is_ascii_alphanumeric() && (i > 0 || !c.is_ascii_digit())
            })
    }
    fn push_line(
        out: &mut String,
        origins: &mut Vec<(PathBuf, usize)>,
        text: &str,
        file: &Path,
        line_no: usize,
    ) {
        out.push_str(text);
        out.push('\n');
        origins.push((file.to_path_buf(), line_no));
    }
    fn load(
        path: &Path,
        seen: &mut HashSet<PathBuf>,
        seen_pkgs: &mut HashSet<String>,
        origins: &mut Vec<(PathBuf, usize)>,
    ) -> Result<String, String> {
        let full = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if !seen.insert(full.clone()) {
            return Ok(String::new());
        }
        let src = fs::read_to_string(&full).map_err(|e| e.to_string())?;
        let base = full.parent().unwrap_or(Path::new("."));
        let mut out = String::new();
        for (idx, line) in src.lines().enumerate() {
            let line_no = idx + 1;
            let mut rest = line.trim();
            let mut had_use = false;
            while rest.starts_with("use ") {
                let Some(a) = rest.find('"') else { break };
                let Some(b_rel) = rest[a + 1..].find('"') else {
                    break;
                };
                let b = a + 1 + b_rel;
                let rel = &rest[a + 1..b];
                let mut is_pkg = false;
                let mut native_only_pkg = false;
                let target = if !rel.contains('/') && !rel.contains('\\') && !rel.ends_with(".vy") {
                    if let Ok(root) = project_root(base) {
                        let manifest_path = root.join("veyra.json");
                        let manifest: JsonValue = serde_json::from_str(
                            &fs::read_to_string(&manifest_path).map_err(|e| e.to_string())?,
                        )
                        .map_err(|e| e.to_string())?;
                        if let Some(meta) = manifest.get("packages").and_then(|p| p.get(rel)) {
                            is_pkg = true;
                            let entry = meta
                                .get("entry")
                                .and_then(JsonValue::as_str)
                                .unwrap_or("main.vy");
                            let candidate = root.join("vendor").join(rel).join(entry);
                            if !candidate.is_file()
                                && root.join("vendor").join(rel).join("veyra.json").is_file()
                            {
                                native_only_pkg = true;
                            }
                            candidate
                        } else {
                            let pkg_manifest = root.join("vendor").join(rel).join("veyra.json");
                            if pkg_manifest.is_file() {
                                is_pkg = true;
                                native_only_pkg = true;
                            }
                            base.join(rel)
                        }
                    } else {
                        base.join(rel)
                    }
                } else {
                    base.join(rel)
                };
                if !native_only_pkg {
                    out.push_str(&load(&target, seen, seen_pkgs, origins)?);
                }
                push_line(&mut out, origins, "", &full, line_no);
                if is_pkg
                    && is_valid_ident(rel)
                    && src.contains(&format!("{rel}."))
                    && !src.contains(&format!("let {rel}"))
                    && seen_pkgs.insert(rel.to_string())
                {
                    push_line(
                        &mut out,
                        origins,
                        &format!("let {rel} = import_package(\"{rel}\");"),
                        &full,
                        line_no,
                    );
                }
                had_use = true;
                let after = rest[b + 1..].trim_start();
                rest = after.strip_prefix(';').unwrap_or(after).trim_start();
            }
            if !had_use {
                push_line(&mut out, origins, line, &full, line_no);
            } else if !rest.is_empty() {
                push_line(&mut out, origins, rest, &full, line_no);
            }
        }
        Ok(out)
    }
    let mut origins = Vec::new();
    let combined = load(
        Path::new(path),
        &mut HashSet::new(),
        &mut HashSet::new(),
        &mut origins,
    )?;
    Ok((combined, origins))
}

pub fn load_source(path: &str) -> Result<String, String> {
    load_source_with_lines(path).map(|(src, _)| src)
}

pub(crate) fn semantic_check_ast(ast: &[Stmt]) -> Result<(), String> {
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
            Expr::Ternary(c, a, b) => {
                check_expr(c, scopes, errors);
                check_expr(a, scopes, errors);
                check_expr(b, scopes, errors);
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
            Expr::CallExpr(callee, args) => {
                check_expr(callee, scopes, errors);
                for a in args {
                    check_expr(a, scopes, errors);
                }
            }
            Expr::CallExt {
                callee,
                method,
                args,
            } => {
                if !(method.is_none() && matches!(&**callee, Expr::Var(_))) {
                    check_expr(callee, scopes, errors);
                }
                for a in args {
                    match a {
                        CallArg::Pos(e)
                        | CallArg::Star(e)
                        | CallArg::Kw(_, e)
                        | CallArg::KwStar(e) => check_expr(e, scopes, errors),
                    }
                }
            }
            Expr::ChainedCmp(first, links) => {
                check_expr(first, scopes, errors);
                for (_, rhs) in links {
                    check_expr(rhs, scopes, errors);
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
                Stmt::SetIndex(target, idx, value) => {
                    check_expr(target, &scopes, errors);
                    check_expr(idx, &scopes, errors);
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
                Stmt::Class(_, parent, body) => {
                    if let Some(p) = parent {
                        if !known(&scopes, p) {
                            errors.push(format!("unknown parent class '{p}'"));
                        }
                    }
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
                Stmt::Return(None) | Stmt::Break | Stmt::Continue | Stmt::SourceLine(_) => {}
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

pub(crate) fn compile_source_unoptimized(src: &str) -> Result<Vec<Op>, String> {
    let t = lex_with_lines(src)?;
    let ast = Parser::new_with_lines(t).program()?;
    semantic_check_ast(&ast)?;
    let mut c = Compiler::new();
    c.emit_stmts(&ast)?;
    Ok(c.finish())
}

pub(crate) fn compile_source_with_debug(src: &str) -> Result<(Vec<Op>, CompiledDebug), String> {
    let t = lex_with_lines(src)?;
    let ast = Parser::new_with_debug_lines(t).program()?;
    semantic_check_ast(&ast)?;
    let mut c = Compiler::new();
    c.emit_stmts(&ast)?;
    Ok(c.finish_with_debug())
}

pub(crate) fn compile_file_with_debug(path: &str) -> Result<(Vec<Op>, DebugMap), String> {
    let (src, origins) = load_source_with_lines(path)?;
    let (code, dbg) = compile_source_with_debug(&src)?;
    Ok((code, DebugMap::from_compiled(dbg, origins)))
}

pub(crate) fn compile_eval_expr(expr: &str) -> Result<Vec<Op>, String> {
    let src = format!("return {expr};");
    let t = lex_with_lines(&src)?;
    let ast = Parser::new_with_lines(t).program()?;
    let mut c = Compiler::new();
    c.emit_stmts(&ast)?;
    Ok(c.finish())
}

pub(crate) fn compile_source(src: &str) -> Result<Vec<Op>, String> {
    Ok(optimize_bytecode(compile_source_unoptimized(src)?))
}

pub(crate) fn prepare_vm(src: &str) -> Result<Vm, String> {
    let mut vm = Vm::new(compile_source(src)?);
    let _ = vm.execute()?;
    Ok(vm)
}

pub fn run_source(src: &str) -> Result<(), String> {
    Vm::new(compile_source(src)?).run()
}

pub fn compile_to_vybc(src: &str) -> Result<String, String> {
    encode_bytecode(&compile_source(src)?)
}

pub fn run_vybc(vybc_text: &str) -> Result<(), String> {
    Vm::new(decode_bytecode(vybc_text)?).run()
}

pub fn disasm_source(src: &str) -> Result<(), String> {
    let code = compile_source(src)?;
    for (i, op) in code.iter().enumerate() {
        println!("{i:04}  {op:?}");
    }
    Ok(())
}
