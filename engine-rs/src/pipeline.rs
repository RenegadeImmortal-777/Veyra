// Compile pipeline: source to bytecode, plus the convenience entry points
// used by the CLI, the LSP and the tests.
//
// Extracted from `main.rs` without behavior change; see docs/ENGINE-SPLIT.md.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value as JsonValue;

use crate::ast::{CallArg, Expr, Pattern, SeqElem, Stmt};
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
                if let Some(py_mod) = rel.strip_prefix("py:") {
                    let after_q = rest[b + 1..].trim_start();
                    let (alias, remainder) = if let Some(after_as) = after_q.strip_prefix("as ") {
                        let a_trim = after_as.trim_start();
                        let end_id = a_trim
                            .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                            .unwrap_or(a_trim.len());
                        (&a_trim[..end_id], &a_trim[end_id..])
                    } else {
                        (py_mod.rsplit('.').next().unwrap_or(py_mod), after_q)
                    };
                    if is_valid_ident(alias)
                        && !src.contains(&format!("let {alias} "))
                        && !src.contains(&format!("let {alias}="))
                        && seen_pkgs.insert(format!("py:{py_mod}:{alias}"))
                    {
                        push_line(
                            &mut out,
                            origins,
                            &format!("let {alias} = py_import(\"{py_mod}\");"),
                            &full,
                            line_no,
                        );
                    } else {
                        push_line(&mut out, origins, "", &full, line_no);
                    }
                    had_use = true;
                    let trimmed = remainder.trim_start();
                    rest = trimmed.strip_prefix(';').unwrap_or(trimmed).trim_start();
                    continue;
                }
                let mut is_pkg = false;
                let mut native_only_pkg = false;
                let is_pkg_spec = !rel.ends_with(".vy")
                    && !rel.contains('\\')
                    && (!rel.contains('/') || rel.starts_with('@'));
                let target = if is_pkg_spec {
                    if let Ok(root) = project_root(base) {
                        let manifest_path = root.join("veyra.json");
                        let manifest: JsonValue = serde_json::from_str(
                            &fs::read_to_string(&manifest_path).map_err(|e| e.to_string())?,
                        )
                        .map_err(|e| e.to_string())?;
                        if let Some(meta) = manifest.get("packages").and_then(|p| p.get(rel)) {
                            is_pkg = true;
                            let vdir = crate::registry::vendor_pkg_dir(&root, rel);
                            let (pkg_dir, ws_entry) = if vdir.is_dir() {
                                (vdir, None)
                            } else if let Some((ws_dir, ws_manifest)) =
                                crate::registry::find_workspace_package(&root, rel)
                            {
                                (
                                    ws_dir,
                                    ws_manifest
                                        .get("entry")
                                        .and_then(JsonValue::as_str)
                                        .map(str::to_string),
                                )
                            } else {
                                (vdir, None)
                            };
                            let entry = meta
                                .get("entry")
                                .and_then(JsonValue::as_str)
                                .or(ws_entry.as_deref())
                                .unwrap_or("main.vy");
                            let candidate = pkg_dir.join(entry);
                            if !candidate.is_file() && pkg_dir.join("veyra.json").is_file() {
                                native_only_pkg = true;
                            }
                            candidate
                        } else if let Some((ws_dir, ws_manifest)) =
                            crate::registry::find_workspace_package(&root, rel)
                        {
                            is_pkg = true;
                            let entry = ws_manifest
                                .get("entry")
                                .and_then(JsonValue::as_str)
                                .unwrap_or("main.vy");
                            let candidate = ws_dir.join(entry);
                            if !candidate.is_file() && ws_dir.join("veyra.json").is_file() {
                                native_only_pkg = true;
                            }
                            candidate
                        } else {
                            let pkg_manifest =
                                crate::registry::vendor_pkg_dir(&root, rel).join("veyra.json");
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

pub(crate) fn is_static_effect_decorator(e: &Expr) -> bool {
    match e {
        Expr::Var(n) => n == "pure",
        Expr::Call(n, _) => n == "pure" || n == "effect",
        Expr::CallExt {
            callee,
            method: None,
            ..
        } => {
            matches!(&**callee, Expr::Var(n) if n == "pure" || n == "effect")
        }
        _ => false,
    }
}

fn const_eval_num(e: &Expr) -> Option<f64> {
    match e {
        Expr::Num(n) => Some(*n),
        Expr::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Expr::Unary(op, x) if op == "-" => Some(-const_eval_num(x)?),
        Expr::Unary(op, x) if op == "+" => const_eval_num(x),
        Expr::Bin(l, op, r) => {
            let a = const_eval_num(l)?;
            let b = const_eval_num(r)?;
            match op.as_str() {
                "+" => Some(a + b),
                "-" => Some(a - b),
                "*" => Some(a * b),
                "/" if b != 0.0 => Some(a / b),
                "%" if b != 0.0 => Some(a % b),
                _ => None,
            }
        }
        _ => None,
    }
}

fn const_eval_bool(e: &Expr) -> Option<bool> {
    match e {
        Expr::Bool(b) => Some(*b),
        Expr::Num(n) => Some(*n != 0.0),
        Expr::Null => Some(false),
        Expr::Unary(op, x) if op == "not" || op == "!" => Some(!const_eval_bool(x)?),
        Expr::Bin(l, op, r) => match op.as_str() {
            "and" => match const_eval_bool(l) {
                Some(false) => Some(false),
                Some(true) => const_eval_bool(r),
                None => None,
            },
            "or" => match const_eval_bool(l) {
                Some(true) => Some(true),
                Some(false) => const_eval_bool(r),
                None => None,
            },
            "==" | "!=" | "<" | "<=" | ">" | ">=" => {
                if let (Some(a), Some(b)) = (const_eval_num(l), const_eval_num(r)) {
                    Some(match op.as_str() {
                        "==" => a == b,
                        "!=" => a != b,
                        "<" => a < b,
                        "<=" => a <= b,
                        ">" => a > b,
                        ">=" => a >= b,
                        _ => unreachable!(),
                    })
                } else {
                    None
                }
            }
            _ => None,
        },
        _ => None,
    }
}

fn builtin_effect_tag(name: &str) -> Option<&'static str> {
    match name {
        "print" | "println" | "input" => Some("io"),
        "read_file" | "write_file" | "append_file" | "remove_file" | "path_exists" | "is_file"
        | "is_dir" | "list_dir" | "mkdir" | "sqlite_open" | "sqlite_exec" | "sqlite_query" => {
            Some("fs")
        }
        "http_request" | "http_get" | "http_post" | "http_serve" | "ws_connect" => Some("net"),
        "shell" | "spawn_process" | "exit" => Some("process"),
        "env_get" | "env_set" | "now_ms" | "time_ms" | "rand" | "rand_int" | "uuid_v4" | "cwd" => {
            Some("env")
        }
        "plugin_load" | "plugin_call" | "plugin_build" | "py_import" | "py_call" | "py_eval"
        | "py_exec" | "wasm_run" => Some("ffi"),
        "gpu_map" | "gpu_reduce" | "gpu_matmul" | "gpu_fuse" => Some("gpu"),
        "actor_spawn" | "actor_send" | "actor_ask" | "actor_stop" | "sim_start" | "sim_stop"
        | "cap_restrict" | "gc_collect" | "gc_pause" | "gc_resume" => Some("state"),
        _ => None,
    }
}

fn verify_effects_and_contracts(ast: &[Stmt], errors: &mut Vec<String>) {
    struct FnEffectInfo {
        ceiling: Option<BTreeSet<String>>,
        direct_effects: BTreeSet<String>,
        callees: BTreeSet<String>,
    }

    fn collect_locals(stmts: &[Stmt], out: &mut HashSet<String>) {
        for s in stmts {
            match s {
                Stmt::Let(n, ..) => {
                    out.insert(n.clone());
                }
                Stmt::ForRange(n, _, _, b) | Stmt::ForEach(n, _, b) | Stmt::With(_, n, b) => {
                    out.insert(n.clone());
                    collect_locals(b, out);
                }
                Stmt::If(_, t, e) => {
                    collect_locals(t, out);
                    collect_locals(e, out);
                }
                Stmt::While(_, b) => {
                    collect_locals(b, out);
                }
                Stmt::Try(b, err, c, f) => {
                    out.insert(err.clone());
                    collect_locals(b, out);
                    collect_locals(c, out);
                    collect_locals(f, out);
                }
                Stmt::Match(_, arms) => {
                    for (_, _, b) in arms {
                        collect_locals(b, out);
                    }
                }
                _ => {}
            }
        }
    }

    fn check_static_contract_call(name: &str, args: &[Expr], errors: &mut Vec<String>) {
        if (name == "requires" || name == "ensures")
            && let Some(cond) = args.first()
            && const_eval_bool(cond) == Some(false)
        {
            let kind = if name == "requires" {
                "precondition"
            } else {
                "postcondition"
            };
            errors.push(format!(
                "contract error: {kind} is statically unsatisfiable"
            ));
        }
    }

    fn scan_expr_effects(
        e: &Expr,
        locals: &HashSet<String>,
        info: &mut FnEffectInfo,
        errors: &mut Vec<String>,
    ) {
        match e {
            Expr::Call(n, args) => {
                check_static_contract_call(n, args, errors);
                if let Some(tag) = builtin_effect_tag(n) {
                    info.direct_effects.insert(tag.to_string());
                } else {
                    info.callees.insert(n.clone());
                }
                for a in args {
                    scan_expr_effects(a, locals, info, errors);
                }
            }
            Expr::CallExt {
                callee,
                method,
                args,
            } => {
                let pos: Vec<Expr> = args
                    .iter()
                    .filter_map(|a| match a {
                        CallArg::Pos(x) | CallArg::Kw(_, x) => Some(x.clone()),
                        _ => None,
                    })
                    .collect();
                if method.is_none()
                    && let Expr::Var(n) = callee.as_ref()
                {
                    check_static_contract_call(n, &pos, errors);
                    if let Some(tag) = builtin_effect_tag(n) {
                        info.direct_effects.insert(tag.to_string());
                    } else {
                        info.callees.insert(n.clone());
                    }
                } else {
                    scan_expr_effects(callee, locals, info, errors);
                }
                for a in args {
                    match a {
                        CallArg::Pos(x)
                        | CallArg::Star(x)
                        | CallArg::Kw(_, x)
                        | CallArg::KwStar(x) => scan_expr_effects(x, locals, info, errors),
                    }
                }
            }
            Expr::List(xs) => {
                for x in xs {
                    scan_expr_effects(x, locals, info, errors);
                }
            }
            Expr::Map(ps) => {
                for (k, v) in ps {
                    scan_expr_effects(k, locals, info, errors);
                    scan_expr_effects(v, locals, info, errors);
                }
            }
            Expr::Index(a, b) | Expr::Bin(a, _, b) => {
                scan_expr_effects(a, locals, info, errors);
                scan_expr_effects(b, locals, info, errors);
            }
            Expr::Ternary(a, b, c) => {
                scan_expr_effects(a, locals, info, errors);
                scan_expr_effects(b, locals, info, errors);
                scan_expr_effects(c, locals, info, errors);
            }
            Expr::Member(o, _) | Expr::Unary(_, o) | Expr::Await(o) => {
                scan_expr_effects(o, locals, info, errors);
            }
            Expr::MethodCall(o, _, args) => {
                scan_expr_effects(o, locals, info, errors);
                for a in args {
                    scan_expr_effects(a, locals, info, errors);
                }
            }
            Expr::ChainedCmp(f, rest) => {
                scan_expr_effects(f, locals, info, errors);
                for (_, r) in rest {
                    scan_expr_effects(r, locals, info, errors);
                }
            }
            Expr::CallExpr(f, args) => {
                scan_expr_effects(f, locals, info, errors);
                for a in args {
                    scan_expr_effects(a, locals, info, errors);
                }
            }
            Expr::Lambda(ps, _, body) => {
                let mut inner_locals = locals.clone();
                for (p, _) in ps {
                    inner_locals.insert(p.clone());
                }
                collect_locals(body, &mut inner_locals);
                scan_stmts_effects(body, &inner_locals, info, errors);
            }
            Expr::ListComp(val, var, it, cond) => {
                scan_expr_effects(it, locals, info, errors);
                let mut inner = locals.clone();
                inner.insert(var.clone());
                scan_expr_effects(val, &inner, info, errors);
                if let Some(c) = cond {
                    scan_expr_effects(c, &inner, info, errors);
                }
            }
            Expr::Num(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Null | Expr::Var(_) => {}
        }
    }

    fn scan_stmts_effects(
        stmts: &[Stmt],
        locals: &HashSet<String>,
        info: &mut FnEffectInfo,
        errors: &mut Vec<String>,
    ) {
        for s in stmts {
            match s {
                Stmt::Let(_, _, e, _) | Stmt::Expr(e) | Stmt::Throw(e) | Stmt::Yield(e) => {
                    scan_expr_effects(e, locals, info, errors);
                }
                Stmt::Assign(target, e) => {
                    if !locals.contains(target) {
                        info.direct_effects.insert("state".into());
                    }
                    scan_expr_effects(e, locals, info, errors);
                }
                Stmt::SetMember(obj, _, val) => {
                    if let Expr::Var(v) = obj
                        && !locals.contains(v)
                    {
                        info.direct_effects.insert("state".into());
                    }
                    scan_expr_effects(obj, locals, info, errors);
                    scan_expr_effects(val, locals, info, errors);
                }
                Stmt::SetIndex(obj, idx, val) => {
                    if let Expr::Var(v) = obj
                        && !locals.contains(v)
                    {
                        info.direct_effects.insert("state".into());
                    }
                    scan_expr_effects(obj, locals, info, errors);
                    scan_expr_effects(idx, locals, info, errors);
                    scan_expr_effects(val, locals, info, errors);
                }
                Stmt::If(c, t, e) => {
                    scan_expr_effects(c, locals, info, errors);
                    scan_stmts_effects(t, locals, info, errors);
                    scan_stmts_effects(e, locals, info, errors);
                }
                Stmt::While(c, b) => {
                    scan_expr_effects(c, locals, info, errors);
                    scan_stmts_effects(b, locals, info, errors);
                }
                Stmt::ForRange(_, a, b, body) => {
                    scan_expr_effects(a, locals, info, errors);
                    scan_expr_effects(b, locals, info, errors);
                    scan_stmts_effects(body, locals, info, errors);
                }
                Stmt::ForEach(_, it, body) | Stmt::With(it, _, body) => {
                    scan_expr_effects(it, locals, info, errors);
                    scan_stmts_effects(body, locals, info, errors);
                }
                Stmt::Try(b, _, c, f) => {
                    scan_stmts_effects(b, locals, info, errors);
                    scan_stmts_effects(c, locals, info, errors);
                    scan_stmts_effects(f, locals, info, errors);
                }
                Stmt::Match(sub, arms) => {
                    scan_expr_effects(sub, locals, info, errors);
                    for (_, g, b) in arms {
                        if let Some(ge) = g {
                            scan_expr_effects(ge, locals, info, errors);
                        }
                        scan_stmts_effects(b, locals, info, errors);
                    }
                }
                Stmt::Return(Some(e)) => scan_expr_effects(e, locals, info, errors),
                Stmt::Return(None)
                | Stmt::Break
                | Stmt::Continue
                | Stmt::SourceLine(_)
                | Stmt::Fn(..)
                | Stmt::AsyncFn(..)
                | Stmt::DecoratedFn(..)
                | Stmt::Class(..) => {}
            }
        }
    }

    fn parse_effect_ceiling(decs: &[Expr], errors: &mut Vec<String>) -> Option<BTreeSet<String>> {
        let mut ceiling: Option<BTreeSet<String>> = None;
        for d in decs {
            match d {
                Expr::Var(n) if n == "pure" => {
                    ceiling.get_or_insert_with(BTreeSet::new);
                }
                Expr::Call(n, args) if n == "pure" && args.is_empty() => {
                    ceiling.get_or_insert_with(BTreeSet::new);
                }
                Expr::Call(n, args) if n == "effect" => {
                    let set = ceiling.get_or_insert_with(BTreeSet::new);
                    for a in args {
                        if let Expr::Str(s) = a {
                            set.insert(s.clone());
                        } else {
                            errors.push(
                                "effect error: @effect(...) requires String capability arguments"
                                    .into(),
                            );
                        }
                    }
                }
                Expr::CallExt {
                    callee,
                    method: None,
                    args,
                } if matches!(&**callee, Expr::Var(n) if n == "pure" || n == "effect") => {
                    let set = ceiling.get_or_insert_with(BTreeSet::new);
                    if matches!(&**callee, Expr::Var(n) if n == "effect") {
                        for a in args {
                            if let CallArg::Pos(Expr::Str(s)) = a {
                                set.insert(s.clone());
                            } else {
                                errors.push(
                                    "effect error: @effect(...) requires String arguments".into(),
                                );
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        ceiling
    }

    fn collect_fns(
        stmts: &[Stmt],
        map: &mut HashMap<String, FnEffectInfo>,
        errors: &mut Vec<String>,
    ) {
        let mut top_only_dummy = FnEffectInfo {
            ceiling: None,
            direct_effects: BTreeSet::new(),
            callees: BTreeSet::new(),
        };
        let top_locals = HashSet::new();
        for s in stmts {
            match s {
                Stmt::Fn(name, params, _, body) | Stmt::AsyncFn(name, params, _, body) => {
                    let mut locals: HashSet<String> =
                        params.iter().map(|(p, _)| p.clone()).collect();
                    collect_locals(body, &mut locals);
                    let mut info = FnEffectInfo {
                        ceiling: None,
                        direct_effects: BTreeSet::new(),
                        callees: BTreeSet::new(),
                    };
                    scan_stmts_effects(body, &locals, &mut info, errors);
                    map.insert(name.clone(), info);
                    collect_fns(body, map, errors);
                }
                Stmt::DecoratedFn(decs, name, params, _, body) => {
                    let ceiling = parse_effect_ceiling(decs, errors);
                    let mut locals: HashSet<String> =
                        params.iter().map(|(p, _)| p.clone()).collect();
                    collect_locals(body, &mut locals);
                    let mut info = FnEffectInfo {
                        ceiling,
                        direct_effects: BTreeSet::new(),
                        callees: BTreeSet::new(),
                    };
                    scan_stmts_effects(body, &locals, &mut info, errors);
                    map.insert(name.clone(), info);
                    collect_fns(body, map, errors);
                }
                Stmt::Class(_, _, _, _, body) => collect_fns(body, map, errors),
                other => scan_stmts_effects(
                    std::slice::from_ref(other),
                    &top_locals,
                    &mut top_only_dummy,
                    errors,
                ),
            }
        }
    }

    let mut fn_map: HashMap<String, FnEffectInfo> = HashMap::new();
    collect_fns(ast, &mut fn_map, errors);

    // Fixed-point transitive effect propagation over user call graph.
    let mut effs: HashMap<String, BTreeSet<String>> = fn_map
        .iter()
        .map(|(k, v)| (k.clone(), v.direct_effects.clone()))
        .collect();
    loop {
        let mut changed = false;
        let names: Vec<String> = fn_map.keys().cloned().collect();
        for caller in names {
            let callees = fn_map[&caller].callees.clone();
            let mut merged = effs[&caller].clone();
            for callee in callees {
                if let Some(ce) = effs.get(&callee) {
                    for e in ce {
                        if merged.insert(e.clone()) {
                            changed = true;
                        }
                    }
                }
            }
            effs.insert(caller, merged);
        }
        if !changed {
            break;
        }
    }

    for (name, info) in &fn_map {
        if let Some(allowed) = &info.ceiling {
            let actual = &effs[name];
            let disallowed: Vec<String> = actual.difference(allowed).cloned().collect();
            if !disallowed.is_empty() {
                if allowed.is_empty() {
                    errors.push(format!(
                        "effect error: @pure function '{name}' performs disallowed effect(s): {}",
                        disallowed.join(", ")
                    ));
                } else {
                    errors.push(format!(
                        "effect error: function '{name}' uses undeclared effect(s): {}",
                        disallowed.join(", ")
                    ));
                }
            }
        }
    }
}

pub(crate) fn semantic_check_ast(ast: &[Stmt]) -> Result<(), String> {
    semantic_check_ast_with_globals(ast, &[])
}

pub(crate) fn semantic_check_ast_with_globals(
    ast: &[Stmt],
    predeclared: &[String],
) -> Result<(), String> {
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
    fn check_pattern(
        pat: &Pattern,
        scopes: &[HashSet<String>],
        bound: &mut HashSet<String>,
        errors: &mut Vec<String>,
    ) {
        match pat {
            Pattern::Wildcard => {}
            Pattern::Capture(name) => {
                bound.insert(name.clone());
            }
            Pattern::Literal(e) => check_expr(e, scopes, errors),
            Pattern::Or(alts) => {
                for alt in alts {
                    let mut alt_bound = bound.clone();
                    check_pattern(alt, scopes, &mut alt_bound, errors);
                }
                if let Some(first) = alts.first() {
                    let mut first_bound = HashSet::new();
                    check_pattern(first, scopes, &mut first_bound, errors);
                    bound.extend(first_bound);
                }
            }
            Pattern::As(inner, name) => {
                check_pattern(inner, scopes, bound, errors);
                bound.insert(name.clone());
            }
            Pattern::Seq(elems) => {
                for el in elems {
                    match el {
                        SeqElem::Pat(p) => check_pattern(p, scopes, bound, errors),
                        SeqElem::Star(Some(name)) => {
                            bound.insert(name.clone());
                        }
                        SeqElem::Star(None) => {}
                    }
                }
            }
            Pattern::Map { entries, rest } => {
                for (k, v) in entries {
                    check_expr(k, scopes, errors);
                    check_pattern(v, scopes, bound, errors);
                }
                if let Some(r) = rest {
                    bound.insert(r.clone());
                }
            }
            Pattern::Class { cls, pos, kw } => {
                check_expr(cls, scopes, errors);
                for p in pos {
                    check_pattern(p, scopes, bound, errors);
                }
                for (_, p) in kw {
                    check_pattern(p, scopes, bound, errors);
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
                | Stmt::Class(n, _, _, _, _) => {
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
                    let mut after_match = HashSet::new();
                    for (pattern, guard, body) in cases {
                        let mut pat_bound = HashSet::new();
                        check_pattern(pattern, &scopes, &mut pat_bound, errors);
                        let mut nested = scopes.clone();
                        nested.push(pat_bound.clone());
                        if let Some(g) = guard {
                            check_expr(g, &nested, errors);
                        }
                        check_block(body, &nested, errors);
                        after_match.extend(pat_bound);
                    }
                    if let Some(cur) = scopes.last_mut() {
                        cur.extend(after_match);
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
                Stmt::DecoratedFn(decorators, _, params, _, body) => {
                    for dec in decorators {
                        if !is_static_effect_decorator(dec) {
                            check_expr(dec, &scopes, errors);
                        }
                    }
                    let mut nested = scopes.clone();
                    nested.push(params.iter().map(|(name, _)| name.clone()).collect());
                    check_block(body, &nested, errors);
                }
                Stmt::Class(_, bases, metaclass, decorators, body) => {
                    for b in bases {
                        check_expr(b, &scopes, errors);
                    }
                    if let Some(m) = metaclass {
                        check_expr(m, &scopes, errors);
                    }
                    for d in decorators {
                        check_expr(d, &scopes, errors);
                    }
                    let mut cls_attr_scope = HashSet::new();
                    for item in body {
                        if let Stmt::Assign(attr, _) = item {
                            cls_attr_scope.insert(attr.clone());
                        }
                    }
                    let mut cls_scopes = scopes.clone();
                    cls_scopes.push(cls_attr_scope);
                    check_block(body, &cls_scopes, errors);
                }
                Stmt::Return(Some(e)) => check_expr(e, &scopes, errors),
                Stmt::Return(None) | Stmt::Break | Stmt::Continue | Stmt::SourceLine(_) => {}
            }
        }
    }
    let mut root = HashSet::new();
    for builtin_name in [
        "object",
        "type",
        "property",
        "staticmethod",
        "classmethod",
        "super",
        "isinstance",
        "issubclass",
        "mro",
        "delattr",
        "getattr",
        "hasattr",
        "setattr",
        "int",
        "float",
        "str",
        "bool",
        "list",
        "dict",
    ] {
        root.insert(builtin_name.to_string());
    }
    for name in predeclared {
        root.insert(name.clone());
    }
    let mut errors = vec![];
    check_block(ast, &[root], &mut errors);
    verify_effects_and_contracts(ast, &mut errors);
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
    compile_source_with_debug_and_globals(src, &[])
}

pub(crate) fn compile_source_with_debug_and_globals(
    src: &str,
    predeclared: &[String],
) -> Result<(Vec<Op>, CompiledDebug), String> {
    let t = lex_with_lines(src)?;
    let ast = Parser::new_with_debug_lines(t).program()?;
    semantic_check_ast_with_globals(&ast, predeclared)?;
    let mut c = Compiler::new();
    c.emit_stmts(&ast)?;
    Ok(c.finish_with_debug())
}

pub(crate) fn compile_notebook_cell_with_globals(
    src: &str,
    predeclared: &[String],
) -> Result<(Vec<Op>, CompiledDebug), String> {
    let t = lex_with_lines(src)?;
    let mut ast = Parser::new_with_debug_lines(t).program()?;
    semantic_check_ast_with_globals(&ast, predeclared)?;
    if !src.trim().ends_with(';') {
        if let Some(last) = ast.last_mut() {
            if let Stmt::Expr(expr) = last.clone() {
                *last = Stmt::Return(Some(expr));
            }
        }
    }
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
