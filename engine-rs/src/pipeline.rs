// Compile pipeline: source to bytecode, plus the convenience entry points
// used by the CLI, the LSP and the tests.
//
// Extracted from `main.rs` without behavior change; see docs/ENGINE-SPLIT.md.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value as JsonValue;

use crate::ast::{Expr, Stmt};
use crate::bytecode::Op;
use crate::compiler::Compiler;
use crate::lexer::lex;
use crate::parser::Parser;
use crate::project_root;
use crate::vm::Vm;

pub(crate) fn load_source(path: &str) -> Result<String, String> {
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

pub(crate) fn compile_source(src: &str) -> Result<Vec<Op>, String> {
    let t = lex(src)?;
    let ast = Parser::new(t).program()?;
    semantic_check_ast(&ast)?;
    let mut c = Compiler::new();
    c.emit_stmts(&ast)?;
    Ok(c.finish())
}
pub(crate) fn prepare_vm(src: &str) -> Result<Vm, String> {
    let mut vm = Vm::new(compile_source(src)?);
    let _ = vm.execute()?;
    Ok(vm)
}

pub(crate) fn run_source(src: &str) -> Result<(), String> {
    Vm::new(compile_source(src)?).run()
}

pub(crate) fn disasm_source(src: &str) -> Result<(), String> {
    let code = compile_source(src)?;
    for (i, op) in code.iter().enumerate() {
        println!("{i:04}  {op:?}");
    }
    Ok(())
}
