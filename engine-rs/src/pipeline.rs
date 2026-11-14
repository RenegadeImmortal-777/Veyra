// Compile pipeline: source to bytecode, plus the convenience entry points
// used by the CLI, the LSP and the tests.
//
// Extracted from `main.rs` without behavior change; see docs/ENGINE-SPLIT.md.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value as JsonValue, json};

use crate::ast::{CallArg, Expr, Pattern, SeqElem, Stmt};
use crate::bytecode::{Op, decode_bytecode, encode_bytecode};
use crate::compiler::{CompiledDebug, Compiler};
use crate::dap::DebugMap;
use crate::ir::{optimize_bytecode, optimize_bytecode_with_stats};
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
                let is_pkg_spec = !rel.ends_with(".kvr")
                    && !rel.contains('\\')
                    && (!rel.contains('/') || rel.starts_with('@'));
                let target = if is_pkg_spec {
                    if let Ok(root) = project_root(base) {
                        let manifest_path = root.join("kovrek.json");
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
                                .unwrap_or("main.kvr");
                            let candidate = pkg_dir.join(entry);
                            if !candidate.is_file() && pkg_dir.join("kovrek.json").is_file() {
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
                                .unwrap_or("main.kvr");
                            let candidate = ws_dir.join(entry);
                            if !candidate.is_file() && ws_dir.join("kovrek.json").is_file() {
                                native_only_pkg = true;
                            }
                            candidate
                        } else {
                            let pkg_manifest =
                                crate::registry::vendor_pkg_dir(&root, rel).join("kovrek.json");
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

fn is_static_marker_name(name: &str) -> bool {
    matches!(
        name,
        "pure" | "effect" | "verify" | "secret" | "tainted" | "linear" | "sanitize"
    )
}

pub(crate) fn is_static_effect_decorator(e: &Expr) -> bool {
    match e {
        Expr::Var(n) => is_static_marker_name(n),
        Expr::Call(n, _) => is_static_marker_name(n),
        Expr::CallExt {
            callee,
            method: None,
            ..
        } => matches!(&**callee, Expr::Var(n) if is_static_marker_name(n)),
        _ => false,
    }
}

fn decorator_string_args(decs: &[Expr], target: &str) -> (bool, Vec<String>) {
    let mut matched = false;
    let mut args_out = Vec::new();
    for d in decs {
        match d {
            Expr::Var(n) if n == target => matched = true,
            Expr::Call(n, args) if n == target => {
                matched = true;
                for a in args {
                    if let Expr::Str(s) = a {
                        args_out.push(s.clone());
                    }
                }
            }
            Expr::CallExt {
                callee,
                method: None,
                args,
            } if matches!(&**callee, Expr::Var(n) if n == target) => {
                matched = true;
                for a in args {
                    if let CallArg::Pos(Expr::Str(s)) = a {
                        args_out.push(s.clone());
                    }
                }
            }
            _ => {}
        }
    }
    (matched, args_out)
}

fn extract_direct_call(e: &Expr) -> Option<(&str, Vec<&Expr>)> {
    match e {
        Expr::Call(n, args) => Some((n.as_str(), args.iter().collect())),
        Expr::CallExt {
            callee,
            method: None,
            args,
        } => {
            if let Expr::Var(n) = callee.as_ref() {
                let pos = args
                    .iter()
                    .filter_map(|a| match a {
                        CallArg::Pos(x) | CallArg::Kw(_, x) => Some(x),
                        _ => None,
                    })
                    .collect();
                Some((n.as_str(), pos))
            } else {
                None
            }
        }
        _ => None,
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
        "print" | "println" | "input" | "gui_app" => Some("io"),
        "read_file" | "write_file" | "append_file" | "remove_file" | "path_exists" | "is_file"
        | "is_dir" | "list_dir" | "mkdir" | "sqlite_open" | "sqlite_exec" | "sqlite_query" => {
            Some("fs")
        }
        "http_request" | "http_get" | "http_post" | "http_serve" | "ws_connect"
        | "actor_node_listen" | "actor_node_stop" | "actor_remote_send" | "actor_remote_ask" => {
            Some("net")
        }
        "shell" | "powershell" | "ps_exec" | "bash_exec" | "spawn_process" | "exit" => {
            Some("process")
        }
        "env_get" | "env_set" | "now_ms" | "time_ms" | "rand" | "rand_int" | "uuid_v4" | "cwd" => {
            Some("env")
        }
        "plugin_load" | "plugin_call" | "plugin_build" | "py_import" | "py_call" | "py_eval"
        | "py_exec" | "wasm_run" => Some("ffi"),
        "gpu_map"
        | "gpu_reduce"
        | "gpu_matmul"
        | "gpu_tiled_matmul"
        | "gpu_fuse"
        | "gpu_fused_linear"
        | "gpu_attention"
        | "gpu_fused_layernorm" => Some("gpu"),
        "actor_spawn" | "actor_send" | "actor_ask" | "actor_stop" | "supervisor_spawn"
        | "sim_start" | "sim_stop" | "sim_actor_spawn" | "sim_actor_send" | "sim_step"
        | "sim_run_until" | "sim_partition" | "sim_fs_write" | "sim_fs_delete" | "sim_fs_sync"
        | "sim_fs_crash" | "profile_start" | "profile_stop" | "cap_restrict" | "gc_collect"
        | "gc_pause" | "gc_resume" | "arena_scope" => Some("state"),
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

fn is_taint_sink(name: &str) -> bool {
    matches!(
        name,
        "print"
            | "println"
            | "http_get"
            | "http_post"
            | "http_request"
            | "write_file"
            | "append_file"
            | "shell"
            | "powershell"
            | "ps_exec"
            | "bash_exec"
            | "spawn_process"
            | "ffi_call"
            | "py_exec"
    )
}

fn verify_taint_security(ast: &[Stmt], errors: &mut Vec<String>) {
    let mut secret_fns: HashSet<String> = HashSet::new();
    let mut secret_fn_params: HashMap<String, Vec<String>> = HashMap::new();
    let mut sanitizer_fns: HashSet<String> = HashSet::new();

    fn collect_taint_sigs(
        stmts: &[Stmt],
        secret_fns: &mut HashSet<String>,
        secret_fn_params: &mut HashMap<String, Vec<String>>,
        sanitizer_fns: &mut HashSet<String>,
    ) {
        for s in stmts {
            match s {
                Stmt::DecoratedFn(decs, name, _, _, body) => {
                    let (is_sec, sec_args) = decorator_string_args(decs, "secret");
                    let (is_tnt, tnt_args) = decorator_string_args(decs, "tainted");
                    if is_sec || is_tnt {
                        let mut pnames = sec_args;
                        pnames.extend(tnt_args);
                        if pnames.is_empty() {
                            secret_fns.insert(name.clone());
                        } else {
                            secret_fn_params.insert(name.clone(), pnames);
                        }
                    }
                    if decorator_string_args(decs, "sanitize").0 {
                        sanitizer_fns.insert(name.clone());
                    }
                    collect_taint_sigs(body, secret_fns, secret_fn_params, sanitizer_fns);
                }
                Stmt::Fn(_, _, _, body)
                | Stmt::AsyncFn(_, _, _, body)
                | Stmt::Class(_, _, _, _, body) => {
                    collect_taint_sigs(body, secret_fns, secret_fn_params, sanitizer_fns);
                }
                _ => {}
            }
        }
    }

    fn expr_is_tainted(
        e: &Expr,
        tainted_vars: &HashSet<String>,
        secret_fns: &HashSet<String>,
        sanitizer_fns: &HashSet<String>,
        errors: &mut Vec<String>,
    ) -> bool {
        if let Some((name, args)) = extract_direct_call(e) {
            let mut arg_tainted = false;
            for a in &args {
                if expr_is_tainted(a, tainted_vars, secret_fns, sanitizer_fns, errors) {
                    arg_tainted = true;
                }
            }
            if is_taint_sink(name) && arg_tainted {
                errors.push(format!(
                    "security error: tainted/secret data flows into sink '{name}' without sanitize()"
                ));
            }
            if name == "sanitize" || sanitizer_fns.contains(name) {
                return false;
            }
            if matches!(name, "secret_mark" | "tainted") || secret_fns.contains(name) {
                return true;
            }
            return arg_tainted;
        }
        match e {
            Expr::Var(n) => tainted_vars.contains(n),
            Expr::List(xs) => xs
                .iter()
                .any(|x| expr_is_tainted(x, tainted_vars, secret_fns, sanitizer_fns, errors)),
            Expr::Map(ps) => ps.iter().any(|(k, v)| {
                expr_is_tainted(k, tainted_vars, secret_fns, sanitizer_fns, errors)
                    || expr_is_tainted(v, tainted_vars, secret_fns, sanitizer_fns, errors)
            }),
            Expr::Bin(a, _, b) | Expr::Index(a, b) => {
                expr_is_tainted(a, tainted_vars, secret_fns, sanitizer_fns, errors)
                    || expr_is_tainted(b, tainted_vars, secret_fns, sanitizer_fns, errors)
            }
            Expr::Ternary(c, a, b) => {
                let tc = expr_is_tainted(c, tainted_vars, secret_fns, sanitizer_fns, errors);
                let ta = expr_is_tainted(a, tainted_vars, secret_fns, sanitizer_fns, errors);
                let tb = expr_is_tainted(b, tainted_vars, secret_fns, sanitizer_fns, errors);
                tc || ta || tb
            }
            Expr::Unary(_, x) | Expr::Await(x) | Expr::Member(x, _) => {
                expr_is_tainted(x, tainted_vars, secret_fns, sanitizer_fns, errors)
            }
            Expr::MethodCall(o, m, args) => {
                let to = expr_is_tainted(o, tainted_vars, secret_fns, sanitizer_fns, errors);
                let mut ta = false;
                for a in args {
                    if expr_is_tainted(a, tainted_vars, secret_fns, sanitizer_fns, errors) {
                        ta = true;
                    }
                }
                if m == "sanitize" { false } else { to || ta }
            }
            Expr::CallExpr(c, args) => {
                let tc = expr_is_tainted(c, tainted_vars, secret_fns, sanitizer_fns, errors);
                let mut ta = false;
                for a in args {
                    if expr_is_tainted(a, tainted_vars, secret_fns, sanitizer_fns, errors) {
                        ta = true;
                    }
                }
                tc || ta
            }
            Expr::ChainedCmp(f, rest) => {
                let tf = expr_is_tainted(f, tainted_vars, secret_fns, sanitizer_fns, errors);
                let mut tr = false;
                for (_, r) in rest {
                    if expr_is_tainted(r, tainted_vars, secret_fns, sanitizer_fns, errors) {
                        tr = true;
                    }
                }
                tf || tr
            }
            Expr::ListComp(v, _, it, cond) => {
                let ti = expr_is_tainted(it, tainted_vars, secret_fns, sanitizer_fns, errors);
                let tv = expr_is_tainted(v, tainted_vars, secret_fns, sanitizer_fns, errors);
                let tc = cond.as_ref().is_some_and(|c| {
                    expr_is_tainted(c, tainted_vars, secret_fns, sanitizer_fns, errors)
                });
                ti || tv || tc
            }
            _ => false,
        }
    }

    fn walk_taint_stmts(
        stmts: &[Stmt],
        tainted_vars: &mut HashSet<String>,
        secret_fns: &HashSet<String>,
        secret_fn_params: &HashMap<String, Vec<String>>,
        sanitizer_fns: &HashSet<String>,
        errors: &mut Vec<String>,
    ) {
        for s in stmts {
            match s {
                Stmt::Let(n, _, e, _) | Stmt::Assign(n, e) => {
                    if expr_is_tainted(e, tainted_vars, secret_fns, sanitizer_fns, errors) {
                        tainted_vars.insert(n.clone());
                    } else {
                        tainted_vars.remove(n);
                    }
                }
                Stmt::Expr(e) | Stmt::Throw(e) | Stmt::Yield(e) | Stmt::Return(Some(e)) => {
                    let _ = expr_is_tainted(e, tainted_vars, secret_fns, sanitizer_fns, errors);
                }
                Stmt::SetMember(o, _, v) => {
                    let _ = expr_is_tainted(o, tainted_vars, secret_fns, sanitizer_fns, errors);
                    let _ = expr_is_tainted(v, tainted_vars, secret_fns, sanitizer_fns, errors);
                }
                Stmt::SetIndex(o, i, v) => {
                    let _ = expr_is_tainted(o, tainted_vars, secret_fns, sanitizer_fns, errors);
                    let _ = expr_is_tainted(i, tainted_vars, secret_fns, sanitizer_fns, errors);
                    let _ = expr_is_tainted(v, tainted_vars, secret_fns, sanitizer_fns, errors);
                }
                Stmt::If(c, t, e) => {
                    let _ = expr_is_tainted(c, tainted_vars, secret_fns, sanitizer_fns, errors);
                    let mut tt = tainted_vars.clone();
                    walk_taint_stmts(
                        t,
                        &mut tt,
                        secret_fns,
                        secret_fn_params,
                        sanitizer_fns,
                        errors,
                    );
                    let mut te = tainted_vars.clone();
                    walk_taint_stmts(
                        e,
                        &mut te,
                        secret_fns,
                        secret_fn_params,
                        sanitizer_fns,
                        errors,
                    );
                    tainted_vars.extend(tt);
                    tainted_vars.extend(te);
                }
                Stmt::While(c, b) => {
                    let _ = expr_is_tainted(c, tainted_vars, secret_fns, sanitizer_fns, errors);
                    walk_taint_stmts(
                        b,
                        tainted_vars,
                        secret_fns,
                        secret_fn_params,
                        sanitizer_fns,
                        errors,
                    );
                }
                Stmt::ForRange(_, a, b, body) => {
                    let _ = expr_is_tainted(a, tainted_vars, secret_fns, sanitizer_fns, errors);
                    let _ = expr_is_tainted(b, tainted_vars, secret_fns, sanitizer_fns, errors);
                    walk_taint_stmts(
                        body,
                        tainted_vars,
                        secret_fns,
                        secret_fn_params,
                        sanitizer_fns,
                        errors,
                    );
                }
                Stmt::ForEach(_, it, body) | Stmt::With(it, _, body) => {
                    let _ = expr_is_tainted(it, tainted_vars, secret_fns, sanitizer_fns, errors);
                    walk_taint_stmts(
                        body,
                        tainted_vars,
                        secret_fns,
                        secret_fn_params,
                        sanitizer_fns,
                        errors,
                    );
                }
                Stmt::Try(b, _, c, f) => {
                    walk_taint_stmts(
                        b,
                        tainted_vars,
                        secret_fns,
                        secret_fn_params,
                        sanitizer_fns,
                        errors,
                    );
                    walk_taint_stmts(
                        c,
                        tainted_vars,
                        secret_fns,
                        secret_fn_params,
                        sanitizer_fns,
                        errors,
                    );
                    walk_taint_stmts(
                        f,
                        tainted_vars,
                        secret_fns,
                        secret_fn_params,
                        sanitizer_fns,
                        errors,
                    );
                }
                Stmt::Fn(_, _, _, body) | Stmt::AsyncFn(_, _, _, body) => {
                    let mut fn_tainted = HashSet::new();
                    walk_taint_stmts(
                        body,
                        &mut fn_tainted,
                        secret_fns,
                        secret_fn_params,
                        sanitizer_fns,
                        errors,
                    );
                }
                Stmt::DecoratedFn(_, name, _, _, body) => {
                    let mut fn_tainted = HashSet::new();
                    if let Some(pnames) = secret_fn_params.get(name) {
                        for p in pnames {
                            fn_tainted.insert(p.clone());
                        }
                    }
                    walk_taint_stmts(
                        body,
                        &mut fn_tainted,
                        secret_fns,
                        secret_fn_params,
                        sanitizer_fns,
                        errors,
                    );
                }
                Stmt::Class(_, _, _, _, body) => {
                    walk_taint_stmts(
                        body,
                        tainted_vars,
                        secret_fns,
                        secret_fn_params,
                        sanitizer_fns,
                        errors,
                    );
                }
                _ => {}
            }
        }
    }

    collect_taint_sigs(
        ast,
        &mut secret_fns,
        &mut secret_fn_params,
        &mut sanitizer_fns,
    );
    let mut top = HashSet::new();
    walk_taint_stmts(
        ast,
        &mut top,
        &secret_fns,
        &secret_fn_params,
        &sanitizer_fns,
        errors,
    );
}

fn verify_linear_resources(ast: &[Stmt], errors: &mut Vec<String>) {
    let mut linear_constructors: HashSet<String> = HashSet::new();
    let mut linear_fn_params: HashMap<String, Vec<String>> = HashMap::new();

    fn collect_linear_sigs(
        stmts: &[Stmt],
        linear_constructors: &mut HashSet<String>,
        linear_fn_params: &mut HashMap<String, Vec<String>>,
    ) {
        for s in stmts {
            match s {
                Stmt::DecoratedFn(decs, name, _, _, body) => {
                    let (has_lin, lin_args) = decorator_string_args(decs, "linear");
                    if has_lin {
                        if lin_args.is_empty() {
                            linear_constructors.insert(name.clone());
                        } else {
                            linear_fn_params.insert(name.clone(), lin_args);
                        }
                    }
                    collect_linear_sigs(body, linear_constructors, linear_fn_params);
                }
                Stmt::Fn(_, _, _, body)
                | Stmt::AsyncFn(_, _, _, body)
                | Stmt::Class(_, _, _, _, body) => {
                    collect_linear_sigs(body, linear_constructors, linear_fn_params);
                }
                _ => {}
            }
        }
    }

    fn consume_var(var: &str, state: &mut HashMap<String, bool>, errors: &mut Vec<String>) {
        match state.get(var).copied() {
            Some(true) => {
                state.insert(var.to_string(), false);
            }
            Some(false) => {
                errors.push(format!(
                    "ownership error: linear resource '{var}' consumed more than once"
                ));
            }
            None => {}
        }
    }

    fn check_linear_expr(
        e: &Expr,
        state: &mut HashMap<String, bool>,
        linear_constructors: &HashSet<String>,
        linear_fn_params: &HashMap<String, Vec<String>>,
        errors: &mut Vec<String>,
    ) -> bool {
        if let Some((name, args)) = extract_direct_call(e) {
            if name == "linear_consume" {
                for a in args {
                    if let Expr::Var(v) = a {
                        consume_var(v, state, errors);
                    } else {
                        let _ = check_linear_expr(
                            a,
                            state,
                            linear_constructors,
                            linear_fn_params,
                            errors,
                        );
                    }
                }
                return false;
            }
            if matches!(
                name,
                "actor_send"
                    | "actor_ask"
                    | "actor_remote_send"
                    | "actor_remote_ask"
                    | "sim_actor_send"
                    | "chan_send"
                    | "channel_send"
            ) {
                for (i, a) in args.iter().enumerate() {
                    if i >= 1
                        && let Expr::Var(v) = a
                        && state.contains_key(v)
                    {
                        consume_var(v, state, errors);
                    } else {
                        let _ = check_linear_expr(
                            a,
                            state,
                            linear_constructors,
                            linear_fn_params,
                            errors,
                        );
                    }
                }
                return false;
            }
            let moves_args =
                linear_constructors.contains(name) || linear_fn_params.contains_key(name);
            for a in args {
                if moves_args && let Expr::Var(v) = a {
                    consume_var(v, state, errors);
                } else {
                    let _ =
                        check_linear_expr(a, state, linear_constructors, linear_fn_params, errors);
                }
            }
            return linear_constructors.contains(name);
        }
        match e {
            Expr::Var(v) => {
                if state.get(v) == Some(&false) {
                    errors.push(format!(
                        "ownership error: linear resource '{v}' used after move/consume"
                    ));
                }
                false
            }
            Expr::MethodCall(o, m, args) => {
                if m == "close"
                    && args.is_empty()
                    && let Expr::Var(v) = o.as_ref()
                {
                    consume_var(v, state, errors);
                    return false;
                }
                let _ = check_linear_expr(o, state, linear_constructors, linear_fn_params, errors);
                for a in args {
                    let _ =
                        check_linear_expr(a, state, linear_constructors, linear_fn_params, errors);
                }
                false
            }
            Expr::List(xs) => {
                for x in xs {
                    let _ =
                        check_linear_expr(x, state, linear_constructors, linear_fn_params, errors);
                }
                false
            }
            Expr::Map(ps) => {
                for (k, v) in ps {
                    let _ =
                        check_linear_expr(k, state, linear_constructors, linear_fn_params, errors);
                    let _ =
                        check_linear_expr(v, state, linear_constructors, linear_fn_params, errors);
                }
                false
            }
            Expr::Bin(a, _, b) | Expr::Index(a, b) => {
                let _ = check_linear_expr(a, state, linear_constructors, linear_fn_params, errors);
                let _ = check_linear_expr(b, state, linear_constructors, linear_fn_params, errors);
                false
            }
            Expr::Ternary(c, a, b) => {
                let _ = check_linear_expr(c, state, linear_constructors, linear_fn_params, errors);
                let _ = check_linear_expr(a, state, linear_constructors, linear_fn_params, errors);
                let _ = check_linear_expr(b, state, linear_constructors, linear_fn_params, errors);
                false
            }
            Expr::Unary(_, x) | Expr::Await(x) | Expr::Member(x, _) => {
                let _ = check_linear_expr(x, state, linear_constructors, linear_fn_params, errors);
                false
            }
            Expr::CallExpr(callee, args) => {
                let _ =
                    check_linear_expr(callee, state, linear_constructors, linear_fn_params, errors);
                for a in args {
                    let _ =
                        check_linear_expr(a, state, linear_constructors, linear_fn_params, errors);
                }
                false
            }
            _ => false,
        }
    }

    fn walk_linear_block(
        stmts: &[Stmt],
        state: &mut HashMap<String, bool>,
        linear_constructors: &HashSet<String>,
        linear_fn_params: &HashMap<String, Vec<String>>,
        errors: &mut Vec<String>,
    ) {
        let mut local_owned: Vec<String> = Vec::new();
        for s in stmts {
            match s {
                Stmt::Let(n, _, e, _) => {
                    let mut produced_lin =
                        check_linear_expr(e, state, linear_constructors, linear_fn_params, errors);
                    if let Expr::Var(src) = e
                        && state.get(src) == Some(&true)
                    {
                        state.insert(src.clone(), false);
                        produced_lin = true;
                    }
                    if produced_lin {
                        state.insert(n.clone(), true);
                        local_owned.push(n.clone());
                    }
                }
                Stmt::Assign(n, e) => {
                    let mut produced_lin =
                        check_linear_expr(e, state, linear_constructors, linear_fn_params, errors);
                    if let Expr::Var(src) = e
                        && state.get(src) == Some(&true)
                    {
                        state.insert(src.clone(), false);
                        produced_lin = true;
                    }
                    if state.get(n) == Some(&true) {
                        errors.push(format!(
                            "ownership error: linear resource '{n}' overwritten before being consumed"
                        ));
                    }
                    if produced_lin {
                        state.insert(n.clone(), true);
                        if !local_owned.contains(n) {
                            local_owned.push(n.clone());
                        }
                    }
                }
                Stmt::Expr(e) | Stmt::Throw(e) | Stmt::Yield(e) => {
                    let _ =
                        check_linear_expr(e, state, linear_constructors, linear_fn_params, errors);
                }
                Stmt::Return(Some(e)) => {
                    if let Expr::Var(v) = e
                        && state.get(v) == Some(&true)
                    {
                        state.insert(v.clone(), false);
                    } else {
                        let _ = check_linear_expr(
                            e,
                            state,
                            linear_constructors,
                            linear_fn_params,
                            errors,
                        );
                    }
                }
                Stmt::If(c, t, e) => {
                    let _ =
                        check_linear_expr(c, state, linear_constructors, linear_fn_params, errors);
                    let outer_live: Vec<String> = state
                        .iter()
                        .filter_map(|(k, v)| if *v { Some(k.clone()) } else { None })
                        .collect();
                    let mut t_st = state.clone();
                    walk_linear_block(t, &mut t_st, linear_constructors, linear_fn_params, errors);
                    let mut e_st = state.clone();
                    walk_linear_block(e, &mut e_st, linear_constructors, linear_fn_params, errors);
                    for k in outer_live {
                        let in_t = t_st.get(&k).copied().unwrap_or(true);
                        let in_e = e_st.get(&k).copied().unwrap_or(true);
                        if in_t != in_e {
                            errors.push(format!(
                                "ownership error: linear resource '{k}' consumed on one branch only"
                            ));
                            state.insert(k, false);
                        } else {
                            state.insert(k, in_t);
                        }
                    }
                }
                Stmt::While(c, b) => {
                    let _ =
                        check_linear_expr(c, state, linear_constructors, linear_fn_params, errors);
                    let outer_live: Vec<String> = state
                        .iter()
                        .filter_map(|(k, v)| if *v { Some(k.clone()) } else { None })
                        .collect();
                    let mut loop_st = state.clone();
                    walk_linear_block(
                        b,
                        &mut loop_st,
                        linear_constructors,
                        linear_fn_params,
                        errors,
                    );
                    for k in outer_live {
                        if loop_st.get(&k).copied() == Some(false) {
                            errors.push(format!(
                                "ownership error: linear resource '{k}' moved inside loop"
                            ));
                            state.insert(k, false);
                        }
                    }
                }
                Stmt::ForRange(_, a, b, body) => {
                    let _ =
                        check_linear_expr(a, state, linear_constructors, linear_fn_params, errors);
                    let _ =
                        check_linear_expr(b, state, linear_constructors, linear_fn_params, errors);
                    walk_linear_block(body, state, linear_constructors, linear_fn_params, errors);
                }
                Stmt::ForEach(_, it, body) | Stmt::With(it, _, body) => {
                    let _ =
                        check_linear_expr(it, state, linear_constructors, linear_fn_params, errors);
                    walk_linear_block(body, state, linear_constructors, linear_fn_params, errors);
                }
                Stmt::Try(b, _, c, f) => {
                    walk_linear_block(b, state, linear_constructors, linear_fn_params, errors);
                    walk_linear_block(c, state, linear_constructors, linear_fn_params, errors);
                    walk_linear_block(f, state, linear_constructors, linear_fn_params, errors);
                }
                Stmt::Fn(_, _, _, body) | Stmt::AsyncFn(_, _, _, body) => {
                    let mut fn_st = HashMap::new();
                    walk_linear_block(
                        body,
                        &mut fn_st,
                        linear_constructors,
                        linear_fn_params,
                        errors,
                    );
                }
                Stmt::DecoratedFn(_, name, _, _, body) => {
                    let mut fn_st = HashMap::new();
                    if let Some(pnames) = linear_fn_params.get(name) {
                        for p in pnames {
                            fn_st.insert(p.clone(), true);
                        }
                    }
                    walk_linear_block(
                        body,
                        &mut fn_st,
                        linear_constructors,
                        linear_fn_params,
                        errors,
                    );
                    for k in fn_st.keys().filter(|k| fn_st.get(*k) == Some(&true)) {
                        errors.push(format!(
                            "ownership error: linear resource '{k}' dropped without being consumed"
                        ));
                    }
                }
                _ => {}
            }
        }
        for name in local_owned {
            if state.get(&name) == Some(&true) {
                errors.push(format!(
                    "ownership error: linear resource '{name}' dropped without being consumed"
                ));
            }
        }
    }

    collect_linear_sigs(ast, &mut linear_constructors, &mut linear_fn_params);
    let mut top = HashMap::new();
    walk_linear_block(
        ast,
        &mut top,
        &linear_constructors,
        &linear_fn_params,
        errors,
    );
}

#[derive(Clone, Debug)]
struct AbsInterval {
    lo: f64,
    hi: f64,
    non_zero: bool,
    list_len: Option<usize>,
}

impl AbsInterval {
    fn any() -> Self {
        Self {
            lo: f64::NEG_INFINITY,
            hi: f64::INFINITY,
            non_zero: false,
            list_len: None,
        }
    }
    fn exact(n: f64) -> Self {
        Self {
            lo: n,
            hi: n,
            non_zero: n != 0.0,
            list_len: None,
        }
    }
    fn list_exact(len: usize) -> Self {
        Self {
            lo: f64::NEG_INFINITY,
            hi: f64::INFINITY,
            non_zero: false,
            list_len: Some(len),
        }
    }
    fn definitely_nonzero(&self) -> bool {
        self.non_zero || self.lo > 0.0 || self.hi < 0.0
    }
    fn join(&self, other: &Self) -> Self {
        let lo = if self.lo.is_nan() || other.lo.is_nan() {
            f64::NEG_INFINITY
        } else {
            self.lo.min(other.lo)
        };
        let hi = if self.hi.is_nan() || other.hi.is_nan() {
            f64::INFINITY
        } else {
            self.hi.max(other.hi)
        };
        let list_len = if self.list_len == other.list_len {
            self.list_len
        } else {
            None
        };
        Self {
            lo,
            hi,
            non_zero: self.definitely_nonzero() && other.definitely_nonzero(),
            list_len,
        }
    }
}

/// Relational difference bounds `(x, y) -> (strict, k)` representing `x - y > k` (strict) or `x - y >= k`.
type RelMap = BTreeMap<(String, String), (bool, f64)>;

fn insert_rel_bound(rels: &mut RelMap, x: String, y: String, strict: bool, k: f64) {
    if x == y {
        return;
    }
    let replace = match rels.get(&(x.clone(), y.clone())) {
        None => true,
        Some(&(old_s, old_k)) => k > old_k || (k == old_k && strict && !old_s),
    };
    if replace {
        rels.insert((x, y), (strict, k));
    }
}

fn intersect_rels(a: &RelMap, b: &RelMap) -> RelMap {
    let mut out = RelMap::new();
    for (k, &(sa, va)) in a {
        if let Some(&(sb, vb)) = b.get(k) {
            if va < vb {
                out.insert(k.clone(), (sa, va));
            } else if vb < va {
                out.insert(k.clone(), (sb, vb));
            } else {
                out.insert(k.clone(), (sa && sb, va));
            }
        }
    }
    out
}

#[derive(Clone, Debug, Default)]
struct VerifySummary {
    functions_checked: usize,
    contracts_proven: usize,
    invariants_checked: usize,
    counterexamples: Vec<JsonValue>,
}

fn interval_from_bounds(raw_lo: f64, raw_hi: f64) -> AbsInterval {
    let lo = if raw_lo.is_nan() {
        f64::NEG_INFINITY
    } else {
        raw_lo
    };
    let hi = if raw_hi.is_nan() {
        f64::INFINITY
    } else {
        raw_hi
    };
    AbsInterval {
        lo,
        hi,
        non_zero: lo > 0.0 || hi < 0.0,
        list_len: None,
    }
}

fn eval_interval(expr: &Expr, env: &HashMap<String, AbsInterval>) -> AbsInterval {
    match expr {
        Expr::Num(n) => AbsInterval::exact(*n),
        Expr::Bool(b) => AbsInterval::exact(if *b { 1.0 } else { 0.0 }),
        Expr::List(xs) => AbsInterval::list_exact(xs.len()),
        Expr::Var(n) => env.get(n).cloned().unwrap_or_else(AbsInterval::any),
        Expr::Unary(op, inner) => {
            let iv = eval_interval(inner, env);
            match op.as_str() {
                "-" => AbsInterval {
                    lo: -iv.hi,
                    hi: -iv.lo,
                    non_zero: iv.non_zero,
                    list_len: None,
                },
                "+" => iv,
                _ => AbsInterval::any(),
            }
        }
        Expr::Bin(l, op, r) => {
            let a = eval_interval(l, env);
            let b = eval_interval(r, env);
            match op.as_str() {
                "+" => interval_from_bounds(a.lo + b.lo, a.hi + b.hi),
                "-" => interval_from_bounds(a.lo - b.hi, a.hi - b.lo),
                "*" if a.lo.is_finite()
                    && a.hi.is_finite()
                    && b.lo.is_finite()
                    && b.hi.is_finite() =>
                {
                    let ps = [a.lo * b.lo, a.lo * b.hi, a.hi * b.lo, a.hi * b.hi];
                    let lo = ps.iter().copied().fold(f64::INFINITY, f64::min);
                    let hi = ps.iter().copied().fold(f64::NEG_INFINITY, f64::max);
                    AbsInterval {
                        lo,
                        hi,
                        non_zero: a.definitely_nonzero() && b.definitely_nonzero(),
                        list_len: None,
                    }
                }
                "/" if b.definitely_nonzero() => {
                    if a.lo.is_finite()
                        && a.hi.is_finite()
                        && b.lo.is_finite()
                        && b.hi.is_finite()
                        && (b.lo > 0.0 || b.hi < 0.0)
                    {
                        let qs = [a.lo / b.lo, a.lo / b.hi, a.hi / b.lo, a.hi / b.hi];
                        let lo = qs.iter().copied().fold(f64::INFINITY, f64::min);
                        let hi = qs.iter().copied().fold(f64::NEG_INFINITY, f64::max);
                        AbsInterval {
                            lo,
                            hi,
                            non_zero: lo > 0.0 || hi < 0.0,
                            list_len: None,
                        }
                    } else if a.lo >= 0.0 && b.lo > 0.0 {
                        let lo = if b.hi.is_finite() { a.lo / b.hi } else { 0.0 };
                        let hi = if a.hi.is_finite() {
                            a.hi / b.lo
                        } else {
                            f64::INFINITY
                        };
                        AbsInterval {
                            lo,
                            hi,
                            non_zero: lo > 0.0,
                            list_len: None,
                        }
                    } else if a.hi <= 0.0 && b.lo > 0.0 {
                        let lo = if a.lo.is_finite() {
                            a.lo / b.lo
                        } else {
                            f64::NEG_INFINITY
                        };
                        let hi = if b.hi.is_finite() { a.hi / b.hi } else { 0.0 };
                        AbsInterval {
                            lo,
                            hi,
                            non_zero: hi < 0.0,
                            list_len: None,
                        }
                    } else {
                        AbsInterval::any()
                    }
                }
                _ => AbsInterval::any(),
            }
        }
        _ => AbsInterval::any(),
    }
}

fn eval_cond_static(cond: &Expr, env: &HashMap<String, AbsInterval>) -> Option<bool> {
    if let Some(b) = const_eval_bool(cond) {
        return Some(b);
    }
    match cond {
        Expr::Unary(op, inner) if op == "not" || op == "!" => {
            eval_cond_static(inner, env).map(|b| !b)
        }
        Expr::Bin(l, op, r) => match op.as_str() {
            "and" => match (eval_cond_static(l, env), eval_cond_static(r, env)) {
                (Some(false), _) | (_, Some(false)) => Some(false),
                (Some(true), Some(true)) => Some(true),
                _ => None,
            },
            "or" => match (eval_cond_static(l, env), eval_cond_static(r, env)) {
                (Some(true), _) | (_, Some(true)) => Some(true),
                (Some(false), Some(false)) => Some(false),
                _ => None,
            },
            "==" | "!=" | "<" | "<=" | ">" | ">=" => {
                let a = eval_interval(l, env);
                let b = eval_interval(r, env);
                if a.lo.is_nan() || a.hi.is_nan() || b.lo.is_nan() || b.hi.is_nan() {
                    return None;
                }
                match op.as_str() {
                    ">" => {
                        if a.lo > b.hi {
                            Some(true)
                        } else if a.hi <= b.lo {
                            Some(false)
                        } else {
                            None
                        }
                    }
                    ">=" => {
                        if a.lo >= b.hi {
                            Some(true)
                        } else if a.hi < b.lo {
                            Some(false)
                        } else {
                            None
                        }
                    }
                    "<" => {
                        if a.hi < b.lo {
                            Some(true)
                        } else if a.lo >= b.hi {
                            Some(false)
                        } else {
                            None
                        }
                    }
                    "<=" => {
                        if a.hi <= b.lo {
                            Some(true)
                        } else if a.lo > b.hi {
                            Some(false)
                        } else {
                            None
                        }
                    }
                    "==" => {
                        if a.lo == a.hi && b.lo == b.hi && a.lo == b.lo {
                            Some(true)
                        } else if a.hi < b.lo || b.hi < a.lo {
                            Some(false)
                        } else {
                            None
                        }
                    }
                    "!=" => {
                        if a.hi < b.lo || b.hi < a.lo {
                            Some(true)
                        } else if a.lo == a.hi && b.lo == b.hi && a.lo == b.lo {
                            Some(false)
                        } else if (b.lo == 0.0 && b.hi == 0.0 && a.definitely_nonzero())
                            || (a.lo == 0.0 && a.hi == 0.0 && b.definitely_nonzero())
                        {
                            Some(true)
                        } else {
                            None
                        }
                    }
                    _ => None,
                }
            }
            _ => None,
        },
        _ => None,
    }
}

fn eval_cond_static_rel(
    cond: &Expr,
    env: &HashMap<String, AbsInterval>,
    rels: &RelMap,
) -> Option<bool> {
    if let Expr::Bin(l, op, r) = cond
        && let (Expr::Var(vx), Expr::Var(vy)) = (l.as_ref(), r.as_ref())
    {
        if let Some(&(strict, c)) = rels.get(&(vx.clone(), vy.clone())) {
            match op.as_str() {
                ">" if c >= 0.0 && (strict || c > 0.0) => return Some(true),
                ">=" if c >= 0.0 => return Some(true),
                "<" if c >= 0.0 => return Some(false),
                "<=" if (strict && c >= 0.0) || c > 0.0 => return Some(false),
                "==" | "!=" if (strict && c >= 0.0) || c > 0.0 => {
                    return Some(op == "!=");
                }
                _ => {}
            }
        }
        if let Some(&(strict, c)) = rels.get(&(vy.clone(), vx.clone())) {
            match op.as_str() {
                "<" if c >= 0.0 && (strict || c > 0.0) => return Some(true),
                "<=" if c >= 0.0 => return Some(true),
                ">" if c >= 0.0 => return Some(false),
                ">=" if (strict && c >= 0.0) || c > 0.0 => return Some(false),
                "==" | "!=" if (strict && c >= 0.0) || c > 0.0 => {
                    return Some(op == "!=");
                }
                _ => {}
            }
        }
    }
    eval_cond_static(cond, env)
}

fn refine_env_and_rels(
    env: &mut HashMap<String, AbsInterval>,
    rels: &mut RelMap,
    cond: &Expr,
    truth: bool,
) {
    match cond {
        Expr::Bin(l, op, r) if truth && op == "and" => {
            refine_env_and_rels(env, rels, l, true);
            refine_env_and_rels(env, rels, r, true);
        }
        Expr::Bin(l, op, r) if !truth && op == "or" => {
            refine_env_and_rels(env, rels, l, false);
            refine_env_and_rels(env, rels, r, false);
        }
        Expr::Bin(l, op, r) => {
            let effective_op = if truth {
                op.as_str()
            } else {
                match op.as_str() {
                    "==" => "!=",
                    "!=" => "==",
                    ">" => "<=",
                    ">=" => "<",
                    "<" => ">=",
                    "<=" => ">",
                    _ => return,
                }
            };
            if let (Expr::Var(vx), Expr::Var(vy)) = (l.as_ref(), r.as_ref()) {
                match effective_op {
                    ">" => insert_rel_bound(rels, vx.clone(), vy.clone(), true, 0.0),
                    ">=" => insert_rel_bound(rels, vx.clone(), vy.clone(), false, 0.0),
                    "<" => insert_rel_bound(rels, vy.clone(), vx.clone(), true, 0.0),
                    "<=" => insert_rel_bound(rels, vy.clone(), vx.clone(), false, 0.0),
                    _ => {}
                }
            }
            refine_env_from_cond(env, cond, truth);
        }
        _ => {}
    }
}

fn eval_interval_rel(
    expr: &Expr,
    env: &HashMap<String, AbsInterval>,
    rels: &RelMap,
) -> AbsInterval {
    if let Expr::Bin(l, op, r) = expr
        && op == "-"
        && let (Expr::Var(vx), Expr::Var(vy)) = (l.as_ref(), r.as_ref())
    {
        if let Some(&(strict, c)) = rels.get(&(vx.clone(), vy.clone())) {
            let lo = if strict { c + 1e-9 } else { c };
            let non_zero = (strict && c >= 0.0) || c > 0.0;
            return AbsInterval {
                lo,
                hi: f64::INFINITY,
                non_zero,
                list_len: None,
            };
        }
        if let Some(&(strict, c)) = rels.get(&(vy.clone(), vx.clone())) {
            let hi = if strict { -c - 1e-9 } else { -c };
            let non_zero = (strict && c >= 0.0) || c > 0.0;
            return AbsInterval {
                lo: f64::NEG_INFINITY,
                hi,
                non_zero,
                list_len: None,
            };
        }
    }
    eval_interval(expr, env)
}

fn collect_expr_vars(e: &Expr, out: &mut BTreeSet<String>) {
    match e {
        Expr::Var(n) => {
            out.insert(n.clone());
        }
        Expr::Bin(l, _, r) | Expr::Index(l, r) => {
            collect_expr_vars(l, out);
            collect_expr_vars(r, out);
        }
        Expr::Unary(_, x) | Expr::Await(x) | Expr::Member(x, _) => collect_expr_vars(x, out),
        Expr::Ternary(c, a, b) => {
            collect_expr_vars(c, out);
            collect_expr_vars(a, out);
            collect_expr_vars(b, out);
        }
        Expr::Call(f, xs) => {
            out.insert(f.clone());
            for x in xs {
                collect_expr_vars(x, out);
            }
        }
        Expr::CallExpr(f, xs) => {
            collect_expr_vars(f, out);
            for x in xs {
                collect_expr_vars(x, out);
            }
        }
        Expr::MethodCall(o, _, xs) => {
            collect_expr_vars(o, out);
            for x in xs {
                collect_expr_vars(x, out);
            }
        }
        _ => {}
    }
}

fn eval_concrete_num(e: &Expr, assign: &BTreeMap<String, f64>) -> Option<f64> {
    match e {
        Expr::Num(n) => Some(*n),
        Expr::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Expr::Var(n) => assign.get(n).copied(),
        Expr::Unary(op, x) if op == "-" => eval_concrete_num(x, assign).map(|v| -v),
        Expr::Unary(op, x) if op == "+" => eval_concrete_num(x, assign),
        Expr::Bin(l, op, r) => {
            let a = eval_concrete_num(l, assign)?;
            let b = eval_concrete_num(r, assign)?;
            match op.as_str() {
                "+" => Some(a + b),
                "-" => Some(a - b),
                "*" => Some(a * b),
                "/" if b != 0.0 => Some(a / b),
                "%" if b != 0.0 => Some(a % b),
                "==" => Some(if (a - b).abs() < 1e-12 { 1.0 } else { 0.0 }),
                "!=" => Some(if (a - b).abs() >= 1e-12 { 1.0 } else { 0.0 }),
                "<" => Some(if a < b { 1.0 } else { 0.0 }),
                "<=" => Some(if a <= b { 1.0 } else { 0.0 }),
                ">" => Some(if a > b { 1.0 } else { 0.0 }),
                ">=" => Some(if a >= b { 1.0 } else { 0.0 }),
                "and" => Some(if a != 0.0 && b != 0.0 { 1.0 } else { 0.0 }),
                "or" => Some(if a != 0.0 || b != 0.0 { 1.0 } else { 0.0 }),
                _ => None,
            }
        }
        _ => None,
    }
}

fn candidate_values_for_interval(iv: &AbsInterval) -> Vec<f64> {
    let mut cands = Vec::new();
    for base in [0.0, 1.0, -1.0, 2.0, -2.0] {
        if base >= iv.lo && base <= iv.hi && !(iv.non_zero && base == 0.0) {
            cands.push(base);
        }
    }
    if iv.lo.is_finite() {
        let v = iv.lo.ceil();
        if v >= iv.lo && v <= iv.hi && !cands.contains(&v) {
            cands.push(v);
        }
    }
    if iv.hi.is_finite() {
        let v = iv.hi.floor();
        if v >= iv.lo && v <= iv.hi && !cands.contains(&v) {
            cands.push(v);
        }
    }
    if cands.is_empty() {
        cands.push(0.0);
    }
    cands
}

fn synthesize_counterexample(
    target_expr: &Expr,
    env: &HashMap<String, AbsInterval>,
    rels: &RelMap,
) -> Option<BTreeMap<String, f64>> {
    let mut vars = BTreeSet::new();
    collect_expr_vars(target_expr, &mut vars);
    if vars.is_empty() || vars.len() > 4 {
        return None;
    }
    let var_list: Vec<String> = vars.into_iter().collect();
    let pools: Vec<Vec<f64>> = var_list
        .iter()
        .map(|v| candidate_values_for_interval(env.get(v).unwrap_or(&AbsInterval::any())))
        .collect();

    let mut cur = BTreeMap::new();
    fn search(
        idx: usize,
        var_list: &[String],
        pools: &[Vec<f64>],
        cur: &mut BTreeMap<String, f64>,
        target_expr: &Expr,
        rels: &RelMap,
    ) -> Option<BTreeMap<String, f64>> {
        if idx == var_list.len() {
            for ((x, y), &(strict, k)) in rels {
                if let (Some(&vx), Some(&vy)) = (cur.get(x), cur.get(y)) {
                    let ok = if strict { vx - vy > k } else { vx - vy >= k };
                    if !ok {
                        return None;
                    }
                }
            }
            if let Some(val) = eval_concrete_num(target_expr, cur)
                && val.abs() < 1e-12
            {
                return Some(cur.clone());
            }
            return None;
        }
        let name = &var_list[idx];
        for &v in &pools[idx] {
            cur.insert(name.clone(), v);
            if let Some(found) = search(idx + 1, var_list, pools, cur, target_expr, rels) {
                return Some(found);
            }
        }
        cur.remove(name);
        None
    }
    search(0, &var_list, &pools, &mut cur, target_expr, rels)
}

fn format_counterexample(cx: &BTreeMap<String, f64>) -> String {
    cx.iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn collect_assigned_vars_in_stmts(stmts: &[Stmt], out: &mut HashSet<String>) {
    for s in stmts {
        match s {
            Stmt::Assign(n, _) => {
                out.insert(n.clone());
            }
            Stmt::If(_, t, e) => {
                collect_assigned_vars_in_stmts(t, out);
                collect_assigned_vars_in_stmts(e, out);
            }
            Stmt::While(_, b) => collect_assigned_vars_in_stmts(b, out),
            Stmt::ForRange(v, _, _, b) | Stmt::ForEach(v, _, b) => {
                out.insert(v.clone());
                collect_assigned_vars_in_stmts(b, out);
            }
            Stmt::Try(b, _, c, f) => {
                collect_assigned_vars_in_stmts(b, out);
                collect_assigned_vars_in_stmts(c, out);
                collect_assigned_vars_in_stmts(f, out);
            }
            _ => {}
        }
    }
}

fn is_monotone_nondec_var_in_stmts(stmts: &[Stmt], var: &str) -> bool {
    let mut ok = true;
    for s in stmts {
        match s {
            Stmt::Assign(n, e) if n == var => {
                let good = matches!(
                    e,
                    Expr::Bin(l, op, r)
                        if matches!(**l, Expr::Var(ref vn) if vn == var)
                            && op == "+"
                            && matches!(**r, Expr::Num(k) if k >= 0.0)
                );
                if !good {
                    return false;
                }
            }
            Stmt::If(_, t, e) => {
                ok &= is_monotone_nondec_var_in_stmts(t, var)
                    && is_monotone_nondec_var_in_stmts(e, var);
            }
            Stmt::While(_, b) | Stmt::ForRange(_, _, _, b) | Stmt::ForEach(_, _, b) => {
                ok &= is_monotone_nondec_var_in_stmts(b, var);
            }
            _ => {}
        }
    }
    ok
}

fn refine_env_from_cond(env: &mut HashMap<String, AbsInterval>, cond: &Expr, truth: bool) {
    match cond {
        Expr::Bin(l, op, r) if truth && op == "and" => {
            refine_env_from_cond(env, l, true);
            refine_env_from_cond(env, r, true);
        }
        Expr::Bin(l, op, r) if !truth && op == "or" => {
            refine_env_from_cond(env, l, false);
            refine_env_from_cond(env, r, false);
        }
        Expr::Bin(l, op, r) => {
            let effective_op = if truth {
                op.as_str()
            } else {
                match op.as_str() {
                    "==" => "!=",
                    "!=" => "==",
                    ">" => "<=",
                    ">=" => "<",
                    "<" => ">=",
                    "<=" => ">",
                    _ => return,
                }
            };
            if let Expr::Var(v) = l.as_ref() {
                let rhs = eval_interval(r, env);
                let mut cur = env.get(v).cloned().unwrap_or_else(AbsInterval::any);
                match effective_op {
                    ">" if rhs.lo.is_finite() => {
                        cur.lo = cur.lo.max(rhs.lo + 1e-9);
                        if cur.lo > 0.0 {
                            cur.non_zero = true;
                        }
                    }
                    ">=" if rhs.lo.is_finite() => {
                        cur.lo = cur.lo.max(rhs.lo);
                        if cur.lo > 0.0 {
                            cur.non_zero = true;
                        }
                    }
                    "<" if rhs.hi.is_finite() => {
                        cur.hi = cur.hi.min(rhs.hi - 1e-9);
                        if cur.hi < 0.0 {
                            cur.non_zero = true;
                        }
                    }
                    "<=" if rhs.hi.is_finite() => {
                        cur.hi = cur.hi.min(rhs.hi);
                        if cur.hi < 0.0 {
                            cur.non_zero = true;
                        }
                    }
                    "==" if rhs.lo == rhs.hi && rhs.lo.is_finite() => {
                        cur.lo = rhs.lo;
                        cur.hi = rhs.lo;
                        cur.non_zero = rhs.lo != 0.0;
                    }
                    "!=" if rhs.lo == 0.0 && rhs.hi == 0.0 => {
                        cur.non_zero = true;
                    }
                    _ => {}
                }
                env.insert(v.clone(), cur);
            }
        }
        _ => {}
    }
}

fn verify_contracts_strict(
    ast: &[Stmt],
    strict_all: bool,
    errors: &mut Vec<String>,
) -> VerifySummary {
    struct FnContractSig {
        params: Vec<String>,
        preconditions: Vec<Expr>,
        postconditions: Vec<Expr>,
        ret_var: Option<String>,
    }

    fn scan_ensures_in_stmts(stmts: &[Stmt], out: &mut Vec<Expr>, ret_var: &mut Option<String>) {
        for s in stmts {
            match s {
                Stmt::Expr(e) => {
                    if let Some((cname, args)) = extract_direct_call(e)
                        && cname == "ensures"
                        && let Some(cond) = args.first()
                    {
                        out.push((*cond).clone());
                    }
                }
                Stmt::Return(Some(Expr::Var(rv))) if ret_var.is_none() => {
                    *ret_var = Some(rv.clone());
                }
                Stmt::If(_, t, e) => {
                    scan_ensures_in_stmts(t, out, ret_var);
                    scan_ensures_in_stmts(e, out, ret_var);
                }
                _ => {}
            }
        }
    }

    let mut fn_sigs: HashMap<String, FnContractSig> = HashMap::new();
    for s in ast {
        let (name, params, body) = match s {
            Stmt::Fn(n, p, _, b) | Stmt::AsyncFn(n, p, _, b) | Stmt::DecoratedFn(_, n, p, _, b) => {
                (n, p, b)
            }
            _ => continue,
        };
        let mut pre = Vec::new();
        for stmt in body {
            if let Stmt::Expr(e) = stmt
                && let Some((cname, args)) = extract_direct_call(e)
                && cname == "requires"
                && let Some(cond) = args.first()
            {
                pre.push((*cond).clone());
            } else if !matches!(stmt, Stmt::SourceLine(_)) {
                break;
            }
        }
        let mut post = Vec::new();
        let mut ret_var = None;
        scan_ensures_in_stmts(body, &mut post, &mut ret_var);
        fn_sigs.insert(
            name.clone(),
            FnContractSig {
                params: params.iter().map(|(pn, _)| pn.clone()).collect(),
                preconditions: pre,
                postconditions: post,
                ret_var,
            },
        );
    }

    fn eval_expr_with_post(
        e: &Expr,
        env: &HashMap<String, AbsInterval>,
        rels: &RelMap,
        fn_sigs: &HashMap<String, FnContractSig>,
    ) -> AbsInterval {
        if let Some((cname, _)) = extract_direct_call(e)
            && let Some(sig) = fn_sigs.get(cname)
            && !sig.postconditions.is_empty()
        {
            let mut post_env: HashMap<String, AbsInterval> = HashMap::new();
            let mut post_rels = RelMap::new();
            for pc in &sig.postconditions {
                refine_env_and_rels(&mut post_env, &mut post_rels, pc, true);
            }
            for key in [
                sig.ret_var.as_deref(),
                Some("result"),
                Some("res"),
                Some("ret"),
            ]
            .into_iter()
            .flatten()
            {
                if let Some(iv) = post_env.get(key) {
                    return iv.clone();
                }
            }
        }
        eval_interval_rel(e, env, rels)
    }

    fn verify_expr_strict(
        e: &Expr,
        env: &HashMap<String, AbsInterval>,
        rels: &RelMap,
        fn_sigs: &HashMap<String, FnContractSig>,
        summary: &mut VerifySummary,
        errors: &mut Vec<String>,
    ) {
        if let Some((cname, args)) = extract_direct_call(e) {
            for a in &args {
                verify_expr_strict(a, env, rels, fn_sigs, summary, errors);
            }
            if let Some(sig) = fn_sigs.get(cname)
                && !sig.preconditions.is_empty()
                && args.len() == sig.params.len()
            {
                let mut callee_env = HashMap::new();
                let callee_rels = RelMap::new();
                for (p, a) in sig.params.iter().zip(args.iter()) {
                    callee_env.insert(p.clone(), eval_expr_with_post(a, env, rels, fn_sigs));
                }
                for pre in &sig.preconditions {
                    summary.invariants_checked += 1;
                    match eval_cond_static_rel(pre, &callee_env, &callee_rels) {
                        Some(false) => {
                            if let Some(cx) =
                                synthesize_counterexample(pre, &callee_env, &callee_rels)
                            {
                                summary
                                    .counterexamples
                                    .push(serde_json::to_value(&cx).unwrap_or_default());
                                errors.push(format!(
                                    "verification error: call to '{cname}' violates requires(...) statically [counterexample: {}]",
                                    format_counterexample(&cx)
                                ));
                            } else {
                                errors.push(format!(
                                    "verification error: call to '{cname}' violates requires(...) statically"
                                ));
                            }
                        }
                        Some(true) => {
                            summary.contracts_proven += 1;
                        }
                        None => {}
                    }
                }
            }
            return;
        }
        match e {
            Expr::Bin(l, op, r) => {
                verify_expr_strict(l, env, rels, fn_sigs, summary, errors);
                verify_expr_strict(r, env, rels, fn_sigs, summary, errors);
                if matches!(op.as_str(), "/" | "%") {
                    summary.invariants_checked += 1;
                    let d = eval_expr_with_post(r, env, rels, fn_sigs);
                    if !d.definitely_nonzero() {
                        if let Some(cx) = synthesize_counterexample(r, env, rels) {
                            summary
                                .counterexamples
                                .push(serde_json::to_value(&cx).unwrap_or_default());
                            errors.push(format!(
                                "verification error: potential division by zero (guard with 'requires(d != 0)' or 'if d != 0') [counterexample: {}]",
                                format_counterexample(&cx)
                            ));
                        } else {
                            errors.push(
                                "verification error: potential division by zero (guard with 'requires(d != 0)' or 'if d != 0')"
                                    .into(),
                            );
                        }
                    }
                }
            }
            Expr::Index(arr, idx) => {
                verify_expr_strict(arr, env, rels, fn_sigs, summary, errors);
                verify_expr_strict(idx, env, rels, fn_sigs, summary, errors);
                let arr_iv = eval_interval_rel(arr, env, rels);
                let idx_iv = eval_interval_rel(idx, env, rels);
                if let Some(len) = arr_iv.list_len
                    && idx_iv.lo.is_finite()
                    && idx_iv.hi.is_finite()
                {
                    summary.invariants_checked += 1;
                    if idx_iv.hi < -(len as f64) || idx_iv.lo >= len as f64 {
                        errors.push(format!(
                            "verification error: static index [{}, {}] out of bounds for list of length {len}",
                            idx_iv.lo, idx_iv.hi
                        ));
                    }
                }
            }
            Expr::List(xs) => {
                for x in xs {
                    verify_expr_strict(x, env, rels, fn_sigs, summary, errors);
                }
            }
            Expr::Map(ps) => {
                for (k, v) in ps {
                    verify_expr_strict(k, env, rels, fn_sigs, summary, errors);
                    verify_expr_strict(v, env, rels, fn_sigs, summary, errors);
                }
            }
            Expr::Ternary(c, a, b) => {
                verify_expr_strict(c, env, rels, fn_sigs, summary, errors);
                let mut t_env = env.clone();
                let mut t_rels = rels.clone();
                refine_env_and_rels(&mut t_env, &mut t_rels, c, true);
                verify_expr_strict(a, &t_env, &t_rels, fn_sigs, summary, errors);
                let mut e_env = env.clone();
                let mut e_rels = rels.clone();
                refine_env_and_rels(&mut e_env, &mut e_rels, c, false);
                verify_expr_strict(b, &e_env, &e_rels, fn_sigs, summary, errors);
            }
            Expr::Unary(_, x) | Expr::Await(x) | Expr::Member(x, _) => {
                verify_expr_strict(x, env, rels, fn_sigs, summary, errors);
            }
            Expr::MethodCall(o, _, args) => {
                verify_expr_strict(o, env, rels, fn_sigs, summary, errors);
                for a in args {
                    verify_expr_strict(a, env, rels, fn_sigs, summary, errors);
                }
            }
            Expr::CallExpr(f, args) => {
                verify_expr_strict(f, env, rels, fn_sigs, summary, errors);
                for a in args {
                    verify_expr_strict(a, env, rels, fn_sigs, summary, errors);
                }
            }
            _ => {}
        }
    }

    fn verify_block_strict(
        stmts: &[Stmt],
        env: &mut HashMap<String, AbsInterval>,
        rels: &mut RelMap,
        fn_sigs: &HashMap<String, FnContractSig>,
        strict_all: bool,
        summary: &mut VerifySummary,
        errors: &mut Vec<String>,
    ) {
        for s in stmts {
            match s {
                Stmt::Let(n, _, e, _) => {
                    verify_expr_strict(e, env, rels, fn_sigs, summary, errors);
                    env.insert(n.clone(), eval_expr_with_post(e, env, rels, fn_sigs));
                }
                Stmt::Assign(n, e) => {
                    verify_expr_strict(e, env, rels, fn_sigs, summary, errors);
                    env.insert(n.clone(), eval_expr_with_post(e, env, rels, fn_sigs));
                    rels.retain(|(x, y), _| x != n && y != n);
                }
                Stmt::Expr(e) => {
                    if let Some((cname, args)) = extract_direct_call(e)
                        && (cname == "requires" || cname == "ensures")
                        && let Some(cond) = args.first()
                    {
                        summary.invariants_checked += 1;
                        match eval_cond_static_rel(cond, env, rels) {
                            Some(false) => {
                                if let Some(cx) = synthesize_counterexample(cond, env, rels) {
                                    summary
                                        .counterexamples
                                        .push(serde_json::to_value(&cx).unwrap_or_default());
                                    errors.push(format!(
                                        "verification error: {cname} contract is statically falsified [counterexample: {}]",
                                        format_counterexample(&cx)
                                    ));
                                } else {
                                    errors.push(format!(
                                        "verification error: {cname} contract is statically falsified"
                                    ));
                                }
                            }
                            Some(true) => {
                                summary.contracts_proven += 1;
                            }
                            None => {}
                        }
                        if cname == "requires" {
                            refine_env_and_rels(env, rels, cond, true);
                        }
                    } else {
                        verify_expr_strict(e, env, rels, fn_sigs, summary, errors);
                    }
                }
                Stmt::Return(Some(e)) | Stmt::Throw(e) | Stmt::Yield(e) => {
                    verify_expr_strict(e, env, rels, fn_sigs, summary, errors);
                }
                Stmt::If(c, t, e) => {
                    verify_expr_strict(c, env, rels, fn_sigs, summary, errors);
                    let mut t_env = env.clone();
                    let mut t_rels = rels.clone();
                    refine_env_and_rels(&mut t_env, &mut t_rels, c, true);
                    verify_block_strict(
                        t,
                        &mut t_env,
                        &mut t_rels,
                        fn_sigs,
                        strict_all,
                        summary,
                        errors,
                    );

                    let mut e_env = env.clone();
                    let mut e_rels = rels.clone();
                    refine_env_and_rels(&mut e_env, &mut e_rels, c, false);
                    verify_block_strict(
                        e,
                        &mut e_env,
                        &mut e_rels,
                        fn_sigs,
                        strict_all,
                        summary,
                        errors,
                    );

                    for k in env.keys().cloned().collect::<Vec<_>>() {
                        let jt = t_env.get(&k).cloned().unwrap_or_else(AbsInterval::any);
                        let je = e_env.get(&k).cloned().unwrap_or_else(AbsInterval::any);
                        env.insert(k, jt.join(&je));
                    }
                    *rels = intersect_rels(&t_rels, &e_rels);
                }
                Stmt::While(c, b) => {
                    let mut assigned = HashSet::new();
                    collect_assigned_vars_in_stmts(b, &mut assigned);
                    let mut loop_env = env.clone();
                    let mut loop_rels = rels.clone();
                    for k in &assigned {
                        let entry = env.get(k).cloned().unwrap_or_else(AbsInterval::any);
                        let widened = if is_monotone_nondec_var_in_stmts(b, k) {
                            AbsInterval {
                                lo: entry.lo,
                                hi: f64::INFINITY,
                                non_zero: entry.lo > 0.0,
                                list_len: None,
                            }
                        } else {
                            AbsInterval::any()
                        };
                        loop_env.insert(k.clone(), widened);
                        loop_rels.retain(|(x, y), _| x != k && y != k);
                    }
                    let mut post_loop_env = loop_env.clone();
                    let mut post_loop_rels = loop_rels.clone();
                    refine_env_and_rels(&mut loop_env, &mut loop_rels, c, true);
                    verify_expr_strict(
                        c,
                        &post_loop_env,
                        &post_loop_rels,
                        fn_sigs,
                        summary,
                        errors,
                    );
                    verify_block_strict(
                        b,
                        &mut loop_env,
                        &mut loop_rels,
                        fn_sigs,
                        strict_all,
                        summary,
                        errors,
                    );
                    refine_env_and_rels(&mut post_loop_env, &mut post_loop_rels, c, false);
                    *env = post_loop_env;
                    *rels = post_loop_rels;
                }
                Stmt::ForRange(v, a, b, body) => {
                    verify_expr_strict(a, env, rels, fn_sigs, summary, errors);
                    verify_expr_strict(b, env, rels, fn_sigs, summary, errors);
                    let mut fenv = env.clone();
                    let mut frels = rels.clone();
                    let a_iv = eval_interval_rel(a, env, rels);
                    let b_iv = eval_interval_rel(b, env, rels);
                    if a_iv.lo.is_finite() && b_iv.hi.is_finite() {
                        fenv.insert(
                            v.clone(),
                            AbsInterval {
                                lo: a_iv.lo,
                                hi: b_iv.hi - 1.0,
                                non_zero: a_iv.lo > 0.0 || b_iv.hi <= 0.0,
                                list_len: None,
                            },
                        );
                    }
                    verify_block_strict(
                        body, &mut fenv, &mut frels, fn_sigs, strict_all, summary, errors,
                    );
                }
                Stmt::Fn(_, params, _, body) | Stmt::AsyncFn(_, params, _, body) if strict_all => {
                    summary.functions_checked += 1;
                    let mut fn_env = HashMap::new();
                    let mut fn_rels = RelMap::new();
                    for (p, _) in params {
                        fn_env.insert(p.clone(), AbsInterval::any());
                    }
                    verify_block_strict(
                        body,
                        &mut fn_env,
                        &mut fn_rels,
                        fn_sigs,
                        strict_all,
                        summary,
                        errors,
                    );
                }
                Stmt::DecoratedFn(decs, _, params, _, body)
                    if strict_all || decorator_string_args(decs, "verify").0 =>
                {
                    summary.functions_checked += 1;
                    let mut fn_env = HashMap::new();
                    let mut fn_rels = RelMap::new();
                    for (p, _) in params {
                        fn_env.insert(p.clone(), AbsInterval::any());
                    }
                    verify_block_strict(
                        body,
                        &mut fn_env,
                        &mut fn_rels,
                        fn_sigs,
                        strict_all,
                        summary,
                        errors,
                    );
                }
                _ => {}
            }
        }
    }

    let mut summary = VerifySummary::default();
    if strict_all {
        let mut top_env = HashMap::new();
        let mut top_rels = RelMap::new();
        verify_block_strict(
            ast,
            &mut top_env,
            &mut top_rels,
            &fn_sigs,
            true,
            &mut summary,
            errors,
        );
    } else {
        fn scan_verify_fns(
            stmts: &[Stmt],
            fn_sigs: &HashMap<String, FnContractSig>,
            summary: &mut VerifySummary,
            errors: &mut Vec<String>,
        ) {
            for s in stmts {
                match s {
                    Stmt::DecoratedFn(decs, _, params, _, body) => {
                        if decorator_string_args(decs, "verify").0 {
                            summary.functions_checked += 1;
                            let mut fn_env = HashMap::new();
                            let mut fn_rels = RelMap::new();
                            for (p, _) in params {
                                fn_env.insert(p.clone(), AbsInterval::any());
                            }
                            verify_block_strict(
                                body,
                                &mut fn_env,
                                &mut fn_rels,
                                fn_sigs,
                                true,
                                summary,
                                errors,
                            );
                        }
                        scan_verify_fns(body, fn_sigs, summary, errors);
                    }
                    Stmt::Fn(_, _, _, body)
                    | Stmt::AsyncFn(_, _, _, body)
                    | Stmt::Class(_, _, _, _, body) => {
                        scan_verify_fns(body, fn_sigs, summary, errors);
                    }
                    _ => {}
                }
            }
        }
        scan_verify_fns(ast, &fn_sigs, &mut summary, errors);
    }
    summary
}

pub(crate) fn verify_source_strict(src: &str) -> Result<JsonValue, String> {
    let t = lex_with_lines(src)?;
    let ast = Parser::new_with_lines(t).program()?;
    let mut errors = Vec::new();
    if let Err(sem_err) = semantic_check_ast(&ast) {
        for part in sem_err
            .strip_prefix("semantic errors: ")
            .unwrap_or(&sem_err)
            .split("; ")
        {
            if !part.is_empty() {
                errors.push(part.to_string());
            }
        }
    }
    let summary = verify_contracts_strict(&ast, true, &mut errors);
    errors.sort();
    errors.dedup();
    Ok(json!({
        "ok": errors.is_empty(),
        "functions_checked": summary.functions_checked,
        "contracts_proven": summary.contracts_proven,
        "invariants_checked": summary.invariants_checked,
        "counterexamples": summary.counterexamples,
        "diagnostics": errors,
    }))
}

pub(crate) fn optimize_source_stats(src: &str) -> Result<JsonValue, String> {
    let unopt = compile_source_unoptimized(src)?;
    let (_, stats) = optimize_bytecode_with_stats(unopt);
    Ok(json!({
        "ops_before": stats.ops_before,
        "ops_after": stats.ops_after,
        "constants_folded": stats.constants_folded,
        "algebraic_simplified": stats.algebraic_simplified,
        "dead_removed": stats.dead_removed,
    }))
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
    verify_taint_security(ast, &mut errors);
    verify_linear_resources(ast, &mut errors);
    let _ = verify_contracts_strict(ast, false, &mut errors);
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

pub(crate) fn analyze_cell_deps(
    src: &str,
) -> (BTreeSet<String>, BTreeSet<String>, BTreeSet<String>) {
    fn collect_pattern_binds(pat: &Pattern, out: &mut HashSet<String>) {
        match pat {
            Pattern::Capture(name) => {
                out.insert(name.clone());
            }
            Pattern::As(inner, name) => {
                out.insert(name.clone());
                collect_pattern_binds(inner, out);
            }
            Pattern::Or(pats) => {
                for p in pats {
                    collect_pattern_binds(p, out);
                }
            }
            Pattern::Seq(elems) => {
                for el in elems {
                    match el {
                        SeqElem::Pat(p) => collect_pattern_binds(p, out),
                        SeqElem::Star(Some(n)) => {
                            out.insert(n.clone());
                        }
                        SeqElem::Star(None) => {}
                    }
                }
            }
            Pattern::Map { entries, rest } => {
                for (_, p) in entries {
                    collect_pattern_binds(p, out);
                }
                if let Some(r) = rest {
                    out.insert(r.clone());
                }
            }
            Pattern::Class { pos, kw, .. } => {
                for p in pos {
                    collect_pattern_binds(p, out);
                }
                for (_, p) in kw {
                    collect_pattern_binds(p, out);
                }
            }
            Pattern::Wildcard | Pattern::Literal(_) => {}
        }
    }

    fn walk_expr(expr: &Expr, bound: &HashSet<String>, refs: &mut BTreeSet<String>) {
        match expr {
            Expr::Num(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Null => {}
            Expr::Var(name) => {
                if !bound.contains(name) {
                    refs.insert(name.clone());
                }
            }
            Expr::List(xs) => {
                for x in xs {
                    walk_expr(x, bound, refs);
                }
            }
            Expr::Map(pairs) => {
                for (k, v) in pairs {
                    walk_expr(k, bound, refs);
                    walk_expr(v, bound, refs);
                }
            }
            Expr::Index(a, b) | Expr::Bin(a, _, b) => {
                walk_expr(a, bound, refs);
                walk_expr(b, bound, refs);
            }
            Expr::Member(obj, _) | Expr::Await(obj) | Expr::Unary(_, obj) => {
                walk_expr(obj, bound, refs);
            }
            Expr::MethodCall(obj, _, args) => {
                walk_expr(obj, bound, refs);
                for a in args {
                    walk_expr(a, bound, refs);
                }
            }
            Expr::Call(name, args) => {
                if !bound.contains(name) {
                    refs.insert(name.clone());
                }
                for a in args {
                    walk_expr(a, bound, refs);
                }
            }
            Expr::CallExpr(callee, args) => {
                walk_expr(callee, bound, refs);
                for a in args {
                    walk_expr(a, bound, refs);
                }
            }
            Expr::CallExt { callee, args, .. } => {
                walk_expr(callee, bound, refs);
                for a in args {
                    match a {
                        CallArg::Pos(e)
                        | CallArg::Star(e)
                        | CallArg::Kw(_, e)
                        | CallArg::KwStar(e) => {
                            walk_expr(e, bound, refs);
                        }
                    }
                }
            }
            Expr::Ternary(c, t, e) => {
                walk_expr(c, bound, refs);
                walk_expr(t, bound, refs);
                walk_expr(e, bound, refs);
            }
            Expr::ChainedCmp(first, rest) => {
                walk_expr(first, bound, refs);
                for (_, e) in rest {
                    walk_expr(e, bound, refs);
                }
            }
            Expr::ListComp(body, var, iter, cond) => {
                walk_expr(iter, bound, refs);
                let mut inner = bound.clone();
                inner.insert(var.clone());
                if let Some(c) = cond {
                    walk_expr(c, &inner, refs);
                }
                walk_expr(body, &inner, refs);
            }
            Expr::Lambda(params, _, body) => {
                let mut inner = bound.clone();
                for (p, _) in params {
                    inner.insert(p.clone());
                }
                let mut dummy_defs = BTreeSet::new();
                let mut dummy_assigns = BTreeSet::new();
                walk_stmts(
                    body,
                    false,
                    &mut inner,
                    &mut dummy_defs,
                    &mut dummy_assigns,
                    refs,
                );
            }
        }
    }

    fn walk_stmts(
        stmts: &[Stmt],
        top_level: bool,
        bound: &mut HashSet<String>,
        defs: &mut BTreeSet<String>,
        assigns: &mut BTreeSet<String>,
        refs: &mut BTreeSet<String>,
    ) {
        for stmt in stmts {
            match stmt {
                Stmt::Let(name, _, init, _) => {
                    walk_expr(init, bound, refs);
                    bound.insert(name.clone());
                    if top_level {
                        defs.insert(name.clone());
                    }
                }
                Stmt::Assign(name, expr) => {
                    walk_expr(expr, bound, refs);
                    if !bound.contains(name) {
                        assigns.insert(name.clone());
                        refs.insert(name.clone());
                    }
                }
                Stmt::Expr(e) | Stmt::Throw(e) | Stmt::Yield(e) => walk_expr(e, bound, refs),
                Stmt::Return(Some(e)) => walk_expr(e, bound, refs),
                Stmt::Return(None) | Stmt::Break | Stmt::Continue | Stmt::SourceLine(_) => {}
                Stmt::SetMember(o, _, v) => {
                    walk_expr(o, bound, refs);
                    walk_expr(v, bound, refs);
                }
                Stmt::SetIndex(o, i, v) => {
                    walk_expr(o, bound, refs);
                    walk_expr(i, bound, refs);
                    walk_expr(v, bound, refs);
                }
                Stmt::If(c, t, e) => {
                    walk_expr(c, bound, refs);
                    walk_stmts(t, false, &mut bound.clone(), defs, assigns, refs);
                    walk_stmts(e, false, &mut bound.clone(), defs, assigns, refs);
                }
                Stmt::While(c, b) => {
                    walk_expr(c, bound, refs);
                    walk_stmts(b, false, &mut bound.clone(), defs, assigns, refs);
                }
                Stmt::ForRange(v, lo, hi, b) => {
                    walk_expr(lo, bound, refs);
                    walk_expr(hi, bound, refs);
                    let mut inner = bound.clone();
                    inner.insert(v.clone());
                    walk_stmts(b, false, &mut inner, defs, assigns, refs);
                }
                Stmt::ForEach(v, iter, b) => {
                    walk_expr(iter, bound, refs);
                    let mut inner = bound.clone();
                    inner.insert(v.clone());
                    walk_stmts(b, false, &mut inner, defs, assigns, refs);
                }
                Stmt::With(e, alias, b) => {
                    walk_expr(e, bound, refs);
                    let mut inner = bound.clone();
                    inner.insert(alias.clone());
                    walk_stmts(b, false, &mut inner, defs, assigns, refs);
                }
                Stmt::Try(tb, err_v, cb, fb) => {
                    walk_stmts(tb, false, &mut bound.clone(), defs, assigns, refs);
                    let mut c_bound = bound.clone();
                    c_bound.insert(err_v.clone());
                    walk_stmts(cb, false, &mut c_bound, defs, assigns, refs);
                    walk_stmts(fb, false, &mut bound.clone(), defs, assigns, refs);
                }
                Stmt::Match(scrut, arms) => {
                    walk_expr(scrut, bound, refs);
                    for (pat, guard, body) in arms {
                        let mut inner = bound.clone();
                        collect_pattern_binds(pat, &mut inner);
                        if let Some(g) = guard {
                            walk_expr(g, &inner, refs);
                        }
                        walk_stmts(body, false, &mut inner, defs, assigns, refs);
                    }
                }
                Stmt::Fn(name, params, _, body) | Stmt::AsyncFn(name, params, _, body) => {
                    bound.insert(name.clone());
                    if top_level {
                        defs.insert(name.clone());
                    }
                    let mut inner = bound.clone();
                    for (p, _) in params {
                        inner.insert(p.clone());
                    }
                    walk_stmts(body, false, &mut inner, defs, assigns, refs);
                }
                Stmt::DecoratedFn(decs, name, params, _, body) => {
                    for d in decs {
                        walk_expr(d, bound, refs);
                    }
                    bound.insert(name.clone());
                    if top_level {
                        defs.insert(name.clone());
                    }
                    let mut inner = bound.clone();
                    for (p, _) in params {
                        inner.insert(p.clone());
                    }
                    walk_stmts(body, false, &mut inner, defs, assigns, refs);
                }
                Stmt::Class(name, decs, base, mixins, body) => {
                    for d in decs {
                        walk_expr(d, bound, refs);
                    }
                    if let Some(b) = base {
                        walk_expr(b, bound, refs);
                    }
                    for m in mixins {
                        walk_expr(m, bound, refs);
                    }
                    bound.insert(name.clone());
                    if top_level {
                        defs.insert(name.clone());
                    }
                    let mut inner = bound.clone();
                    walk_stmts(body, false, &mut inner, defs, assigns, refs);
                }
            }
        }
    }

    let mut defs = BTreeSet::new();
    let mut assigns = BTreeSet::new();
    let mut refs = BTreeSet::new();
    let Ok(tokens) = lex_with_lines(src) else {
        return (defs, assigns, refs);
    };
    let Ok(ast) = Parser::new_with_debug_lines(tokens).program() else {
        return (defs, assigns, refs);
    };
    let mut bound = HashSet::new();
    walk_stmts(&ast, true, &mut bound, &mut defs, &mut assigns, &mut refs);
    (defs, assigns, refs)
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

pub fn compile_to_kvbc(src: &str) -> Result<String, String> {
    encode_bytecode(&compile_source(src)?)
}

pub fn run_kvbc(kvbc_text: &str) -> Result<(), String> {
    Vm::new(decode_bytecode(kvbc_text)?).run()
}

pub fn disasm_source(src: &str) -> Result<(), String> {
    let code = compile_source(src)?;
    for (i, op) in code.iter().enumerate() {
        println!("{i:04}  {op:?}");
    }
    Ok(())
}
