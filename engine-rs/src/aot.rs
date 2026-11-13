// Native AOT compiler backend: compiles Veyra AST to standalone optimized
// Rust machine code via `rustc`. Works offline with zero extra dependencies
// and compiles cleanly under `--no-default-features`.

use std::{
    collections::{BTreeSet, HashMap},
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

use crate::ast::{Expr, Stmt};
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

fn validate_aot_support(stmts: &[Stmt]) -> Result<(), String> {
    for stmt in stmts {
        match stmt {
            Stmt::AsyncFn(..) => {
                return Err("native AOT compilation does not support 'async fn' yet".into());
            }
            Stmt::Yield(_) => {
                return Err(
                    "native AOT compilation does not support 'yield' generators yet".into(),
                );
            }
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
                for (p, b) in arms {
                    if let Some(pe) = p {
                        validate_aot_expr(pe)?;
                    }
                    validate_aot_support(b)?;
                }
            }
            Stmt::Fn(_, _, _, body) | Stmt::DecoratedFn(_, _, _, _, body) => {
                validate_aot_support(body)?;
            }
            Stmt::Class(_, _, methods) => validate_aot_support(methods)?,
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
            Stmt::Return(Some(e)) => validate_aot_expr(e)?,
            Stmt::Return(None) | Stmt::Break | Stmt::Continue | Stmt::SourceLine(_) => {}
        }
    }
    Ok(())
}

fn validate_aot_expr(expr: &Expr) -> Result<(), String> {
    match expr {
        Expr::Await(_) => Err("native AOT compilation does not support 'await' yet".into()),
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
                for (_, b) in arms {
                    collect_scope_decls(b, is_top, decls, explicit_decls, strict, enclosing);
                }
            }
            Stmt::Fn(name, _, _, _) | Stmt::DecoratedFn(_, name, _, _, _) => {
                if !is_top {
                    decls.insert(name.clone());
                    explicit_decls.insert(name.clone());
                }
            }
            _ => {}
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
            Stmt::Fn(n, _, _, body)
            | Stmt::AsyncFn(n, _, _, body)
            | Stmt::DecoratedFn(_, n, _, _, body) => {
                out.insert(n.clone());
                collect_all_ids_stmts(body, out);
            }
            Stmt::Class(_, _, methods) => collect_all_ids_stmts(methods, out),
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
                for (p, b) in arms {
                    if let Some(pe) = p {
                        collect_all_ids_expr(pe, out);
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
            Stmt::Fn(name, _, _, body) => {
                out.insert(name.clone());
                collect_all_ids_stmts(body, out);
            }
            Stmt::AsyncFn(name, _, _, body) | Stmt::DecoratedFn(_, name, _, _, body) => {
                out.insert(name.clone());
                collect_all_ids_stmts(body, out);
            }
            Stmt::Class(_, _, methods) => collect_all_ids_stmts(methods, out),
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
                for (p, b) in arms {
                    if let Some(pe) = p {
                        collect_nested_refs_expr(pe, out);
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
}

impl AotEmitter {
    fn new() -> Self {
        Self {
            indent: 0,
            out: String::new(),
            temp_counter: 0,
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
    ) -> ScopeInfo {
        let mut enclosing = parent_cells.clone();
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
    ) -> Result<String, String> {
        let child_scope = self.build_scope(false, params, stmts, &parent_scope.captured_cells);
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
            "let {fn_var} = rt.reg_fn(Rc::new(move |rt: &mut Rt, mut vy_args: Vec<Val>| -> Result<Val, Flow> {{"
        ));
        self.indent += 1;
        for (ident, cap_tmp) in &captures {
            self.line(&format!("let {ident} = {cap_tmp}.clone();"));
        }
        self.line(&format!("rt.bind_params(&mut vy_args, &{spec_var})?;"));
        self.line("let mut vy_arg_it = vy_args.into_iter();");
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
                    Some(t) if t != "*" && t != "?default" => {
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
        self.line("let vy_body_res: Result<(), Flow> = (|| -> Result<(), Flow> {");
        self.indent += 1;
        self.emit_stmts(stmts, &child_scope)?;
        self.line("Ok(())");
        self.indent -= 1;
        self.line("})();");
        self.line("let vy_ret_val = match vy_body_res {");
        self.line("    Ok(()) => Val::Null,");
        self.line("    Err(Flow::Ret(v)) => v,");
        self.line("    Err(Flow::Break) => return Err(Flow::Err(\"break outside loop\".into())),");
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
        self.line("}));");
        Ok(fn_var)
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
                let fn_v = self.emit_fn_body_value(params, ret_ty, body, scope)?;
                self.line(&format!("let {tmp} = {fn_v};"));
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
            Expr::Await(_) => {
                return Err("native AOT compilation does not support 'await' yet".into());
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
                let fn_v = self.emit_fn_body_value(params, ret_ty, body, scope)?;
                if scope.is_top {
                    self.line(&format!(
                        "if let Val::Fn(vy_fid) = {fn_v} {{ rt.named_fns.insert(String::from({}), vy_fid); }}",
                        escape_rust_str(name)
                    ));
                } else {
                    self.emit_write_var(scope, name, &fn_v, true);
                }
            }
            Stmt::DecoratedFn(dec, name, params, ret_ty, body) => {
                let fn_v = self.emit_fn_body_value(params, ret_ty, body, scope)?;
                let local_opt = match scope.resolve(dec) {
                    VarKind::Plain | VarKind::Cell | VarKind::Captured => {
                        format!("Some({})", self.emit_read_var(scope, dec))
                    }
                    VarKind::Global => "None".to_string(),
                };
                let wrapped = self.fresh_tmp("dec");
                self.line(&format!(
                    "let {wrapped} = rt.call_target({}, {local_opt}, vec![{fn_v}])?;",
                    escape_rust_str(dec)
                ));
                if scope.is_top {
                    self.line(&format!(
                        "if let Val::Fn(vy_fid) = {wrapped} {{ rt.named_fns.insert(String::from({}), vy_fid); }} else {{ rt.set_global({}, {wrapped}); }}",
                        escape_rust_str(name),
                        escape_rust_str(name)
                    ));
                } else {
                    self.emit_write_var(scope, name, &wrapped, true);
                }
            }
            Stmt::Class(name, parent, methods) => {
                let p_expr = match parent {
                    Some(p) => format!("Some(String::from({}))", escape_rust_str(p)),
                    None => "None".to_string(),
                };
                let cls_tmp = self.fresh_tmp("cls_methods");
                self.line(&format!(
                    "let mut {cls_tmp}: HashMap<String, usize> = HashMap::new();"
                ));
                for item in methods {
                    match item {
                        Stmt::SourceLine(_) => {}
                        Stmt::Fn(method_name, params, ret_ty, body) => {
                            let fn_v = self.emit_fn_body_value(params, ret_ty, body, scope)?;
                            self.line(&format!(
                                "if let Val::Fn(vy_mid) = {fn_v} {{ {cls_tmp}.insert(String::from({}), vy_mid); }}",
                                escape_rust_str(method_name)
                            ));
                        }
                        Stmt::AsyncFn(..) => {
                            return Err(
                                "native AOT compilation does not support async methods yet".into(),
                            );
                        }
                        _ => {}
                    }
                }
                self.line(&format!(
                    "rt.define_class(String::from({}), {p_expr}, {cls_tmp})?;",
                    escape_rust_str(name)
                ));
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
                for (pat_opt, body) in arms {
                    if let Some(pat) = pat_opt {
                        let pv = self.emit_expr(pat, scope)?;
                        self.line(&format!("if {sv} == {pv} {{"));
                        self.indent += 1;
                        self.emit_stmts(body, scope)?;
                        self.line(&format!("break '{lbl};"));
                        self.indent -= 1;
                        self.line("}");
                    } else {
                        self.emit_stmts(body, scope)?;
                        self.line(&format!("break '{lbl};"));
                    }
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
            Stmt::AsyncFn(..) => {
                return Err("native AOT compilation does not support 'async fn' yet".into());
            }
            Stmt::Yield(_) => {
                return Err(
                    "native AOT compilation does not support 'yield' generators yet".into(),
                );
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
    emitter.line("fn vy_main(rt: &mut Rt) -> Result<(), Flow> {");
    emitter.indent = 1;
    let empty_parent = BTreeSet::new();
    let root_scope = emitter.build_scope(true, &[], &ast, &empty_parent);
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
    let tmp_rs = env::temp_dir().join(format!("veyra_aot_{pid}_{id}.rs"));
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

const AOT_PREAMBLE: &str = r#"#![allow(
    dead_code,
    unused_variables,
    unused_mut,
    unreachable_code,
    unused_imports,
    clippy::all
)]

use std::cell::RefCell;
use std::collections::HashMap;
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
            Val::Object(_) | Val::Fn(_) => true,
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
            Val::Fn(id) => format!("<function:{id}>"),
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
    parent: Option<String>,
    methods: HashMap<String, usize>,
}

type NativeFn = Rc<dyn Fn(&mut Rt, Vec<Val>) -> Result<Val, Flow>>;

struct Rt {
    fns: Vec<NativeFn>,
    named_fns: HashMap<String, usize>,
    classes: HashMap<String, ClassInfo>,
    objects: Vec<ObjData>,
    globals: HashMap<String, Val>,
    start: Instant,
}

impl Rt {
    fn new() -> Self {
        Self {
            fns: Vec::new(),
            named_fns: HashMap::new(),
            classes: HashMap::new(),
            objects: Vec::new(),
            globals: HashMap::new(),
            start: Instant::now(),
        }
    }

    fn reg_fn(&mut self, f: NativeFn) -> Val {
        let id = self.fns.len();
        self.fns.push(f);
        Val::Fn(id)
    }

    fn set_global(&mut self, name: &str, v: Val) {
        self.globals.insert(name.to_string(), v);
    }

    fn resolve_ident(&self, name: &str) -> Result<Val, Flow> {
        if let Some(v) = self.globals.get(name) {
            return Ok(v.clone());
        }
        if let Some(&id) = self.named_fns.get(name) {
            return Ok(Val::Fn(id));
        }
        Err(Flow::Err(format!("undefined name '{name}'")))
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
        self.classes.insert(name, ClassInfo { parent, methods });
        Ok(())
    }

    fn class_is_a(&self, class_name: &str, target: &str) -> bool {
        let mut cur = Some(class_name.to_string());
        while let Some(name) = cur {
            if name == target {
                return true;
            }
            cur = self.classes.get(&name).and_then(|c| c.parent.clone());
        }
        false
    }

    fn find_method(&self, class_name: &str, method: &str) -> Option<usize> {
        let mut cur = Some(class_name.to_string());
        while let Some(name) = cur {
            let cls = self.classes.get(&name)?;
            if let Some(&id) = cls.methods.get(method) {
                return Some(id);
            }
            cur = cls.parent.clone();
        }
        None
    }

    fn check_type(&self, value: &Val, ty: &str, kind: &str, name: &str) -> Result<(), Flow> {
        let ok = match ty {
            "Any" => true,
            "Number" | "Float" => matches!(value, Val::Num(_)),
            "Int" => matches!(value, Val::Num(n) if n.fract() == 0.0),
            "String" => matches!(value, Val::Str(_)),
            "Bool" => matches!(value, Val::Bool(_)),
            "List" => matches!(value, Val::List(_)),
            "Map" => matches!(value, Val::Map(_)),
            "Null" | "Void" => matches!(value, Val::Null),
            "Object" => matches!(value, Val::Object(_)),
            "Function" => matches!(value, Val::Fn(_)),
            custom => match value {
                Val::Object(id) => self
                    .objects
                    .get(*id)
                    .map(|o| self.class_is_a(&o.class_name, custom))
                    .unwrap_or(false),
                _ => false,
            },
        };
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

    fn bind_params(&self, args: &mut Vec<Val>, params: &[(&str, Option<&str>)]) -> Result<(), Flow> {
        let has_varargs = params.last().and_then(|(_, ty)| *ty) == Some("*");
        if has_varargs {
            let fixed = params.len().saturating_sub(1);
            if args.len() < fixed {
                return Err(Flow::Err(format!(
                    "function expects at least {fixed} arguments, got {}",
                    args.len()
                )));
            }
            let rest = args.split_off(fixed);
            args.push(Val::List(rest));
        } else if args.len() < params.len() {
            let missing_optional = params[args.len()..]
                .iter()
                .all(|(_, ty)| *ty == Some("?default"));
            if missing_optional {
                args.resize(params.len(), Val::Null);
            } else {
                return Err(Flow::Err(format!(
                    "function expects {} arguments, got {}",
                    params.len(),
                    args.len()
                )));
            }
        } else if params.len() != args.len() {
            return Err(Flow::Err(format!(
                "function expects {} arguments, got {}",
                params.len(),
                args.len()
            )));
        }
        for ((param, ty), value) in params.iter().zip(args.iter()) {
            if let Some(expected) = ty {
                if *expected != "*" && *expected != "?default" {
                    self.check_type(value, expected, "argument", param)?;
                }
            }
        }
        Ok(())
    }

    fn call_fn_id(&mut self, id: usize, args: Vec<Val>) -> Result<Val, Flow> {
        let f = self
            .fns
            .get(id)
            .cloned()
            .ok_or_else(|| Flow::Err(format!("dangling function id {id}")))?;
        f(self, args)
    }

    fn call_val(&mut self, val: &Val, args: Vec<Val>) -> Result<Val, Flow> {
        match val {
            Val::Fn(id) => self.call_fn_id(*id, args),
            Val::Str(name) => {
                if let Some(&id) = self.named_fns.get(name) {
                    self.call_fn_id(id, args)
                } else {
                    Err(Flow::Err(format!("unknown function '{name}'")))
                }
            }
            _ => Err(Flow::Err("value is not callable".into())),
        }
    }

    fn instantiate(&mut self, class_name: &str, args: Vec<Val>) -> Result<Val, Flow> {
        let id = self.objects.len();
        self.objects.push(ObjData {
            class_name: class_name.to_string(),
            fields: HashMap::new(),
        });
        if let Some(init_id) = self.find_method(class_name, "init") {
            let mut call_args = vec![Val::Object(id)];
            call_args.extend(args);
            let _ = self.call_fn_id(init_id, call_args)?;
        } else if !args.is_empty() {
            return Err(Flow::Err(format!("{class_name} has no init method")));
        }
        Ok(Val::Object(id))
    }

    #[inline(always)]
    fn call_target(
        &mut self,
        name: &str,
        local_val: Option<Val>,
        args: Vec<Val>,
    ) -> Result<Val, Flow> {
        if let Some(res) = self.call_builtin(name, &args)? {
            return Ok(res);
        }
        if self.classes.contains_key(name) {
            return self.instantiate(name, args);
        }
        if let Some(Val::Fn(id)) = local_val {
            return self.call_fn_id(id, args);
        }
        if let Some(Val::Str(target)) = local_val {
            return self.call_val(&Val::Str(target), args);
        }
        if let Some(Val::Fn(id)) = self.globals.get(name).cloned() {
            return self.call_fn_id(id, args);
        }
        if let Some(&id) = self.named_fns.get(name) {
            return self.call_fn_id(id, args);
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
    fn bin_add(&self, a: Val, b: Val) -> Result<Val, Flow> {
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
    fn bin_sub(&self, a: Val, b: Val) -> Result<Val, Flow> {
        match (a, b) {
            (Val::Num(x), Val::Num(y)) => Ok(Val::Num(x - y)),
            _ => Err(Flow::Err("numeric operation requires numbers".into())),
        }
    }

    #[inline(always)]
    fn bin_mul(&self, a: Val, b: Val) -> Result<Val, Flow> {
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
    fn bin_eq(&self, a: Val, b: Val) -> Result<Val, Flow> {
        Ok(Val::Bool(a == b))
    }

    #[inline(always)]
    fn bin_ne(&self, a: Val, b: Val) -> Result<Val, Flow> {
        Ok(Val::Bool(a != b))
    }

    #[inline(always)]
    fn bin_lt(&self, a: Val, b: Val) -> Result<Val, Flow> {
        match (a, b) {
            (Val::Num(x), Val::Num(y)) => Ok(Val::Bool(x < y)),
            _ => Err(Flow::Err("comparison requires numbers".into())),
        }
    }

    #[inline(always)]
    fn bin_le(&self, a: Val, b: Val) -> Result<Val, Flow> {
        match (a, b) {
            (Val::Num(x), Val::Num(y)) => Ok(Val::Bool(x <= y)),
            _ => Err(Flow::Err("comparison requires numbers".into())),
        }
    }

    #[inline(always)]
    fn bin_gt(&self, a: Val, b: Val) -> Result<Val, Flow> {
        match (a, b) {
            (Val::Num(x), Val::Num(y)) => Ok(Val::Bool(x > y)),
            _ => Err(Flow::Err("comparison requires numbers".into())),
        }
    }

    #[inline(always)]
    fn bin_ge(&self, a: Val, b: Val) -> Result<Val, Flow> {
        match (a, b) {
            (Val::Num(x), Val::Num(y)) => Ok(Val::Bool(x >= y)),
            _ => Err(Flow::Err("comparison requires numbers".into())),
        }
    }

    fn val_index(&self, obj: Val, key: Val) -> Result<Val, Flow> {
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

    fn val_set_index(&self, obj: Val, key: Val, value: Val) -> Result<Val, Flow> {
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

    fn get_member(&self, obj: Val, name: &str) -> Result<Val, Flow> {
        match obj {
            Val::Object(id) => {
                let data = self
                    .objects
                    .get(id)
                    .ok_or_else(|| Flow::Err("dangling object".into()))?;
                data.fields
                    .get(name)
                    .cloned()
                    .ok_or_else(|| Flow::Err(format!("undefined field '{name}'")))
            }
            Val::Map(pairs) => pairs
                .iter()
                .find(|(k, _)| *k == Val::Str(name.to_string()))
                .map(|(_, v)| v.clone())
                .ok_or_else(|| Flow::Err(format!("undefined member '{name}'"))),
            _ => Err(Flow::Err("member access requires object or map".into())),
        }
    }

    fn set_member(&mut self, obj: Val, name: &str, value: Val) -> Result<(), Flow> {
        match obj {
            Val::Object(id) => {
                let data = self
                    .objects
                    .get_mut(id)
                    .ok_or_else(|| Flow::Err("dangling object".into()))?;
                data.fields.insert(name.to_string(), value);
                Ok(())
            }
            _ => Err(Flow::Err("member assignment requires object".into())),
        }
    }

    fn to_iter(&self, val: Val) -> Result<Vec<Val>, Flow> {
        match val {
            Val::List(xs) => Ok(xs),
            Val::Str(s) => Ok(s.chars().map(|c| Val::Str(c.to_string())).collect()),
            Val::Map(pairs) => Ok(pairs.into_iter().map(|(k, _)| k).collect()),
            _ => Err(Flow::Err("value is not iterable".into())),
        }
    }

    fn call_method(&mut self, recv: Val, name: &str, args: Vec<Val>) -> Result<Val, Flow> {
        match recv {
            Val::Object(id) => {
                let class_name = self
                    .objects
                    .get(id)
                    .ok_or_else(|| Flow::Err("dangling object".into()))?
                    .class_name
                    .clone();
                let method_id = self
                    .find_method(&class_name, name)
                    .ok_or_else(|| Flow::Err(format!("undefined method '{name}' on {class_name}")))?;
                let mut call_args = vec![Val::Object(id)];
                call_args.extend(args);
                self.call_fn_id(method_id, call_args)
            }
            Val::Map(pairs) => {
                if let Some((_, member)) = pairs.iter().find(|(k, _)| *k == Val::Str(name.into())) {
                    if matches!(member, Val::Fn(_) | Val::Str(_)) {
                        return self.call_val(&member.clone(), args);
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
                    "len" => Ok(Val::Num(pairs.len() as f64)),
                    _ => Err(Flow::Err(format!("undefined module member '{name}'"))),
                }
            }
            Val::Str(s) => match name {
                "upper" => Ok(Val::Str(s.to_uppercase())),
                "lower" => Ok(Val::Str(s.to_lowercase())),
                "trim" | "strip" => Ok(Val::Str(s.trim().to_string())),
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
                "count" => match args.first() {
                    Some(Val::Str(sub)) => Ok(Val::Num(s.matches(sub.as_str()).count() as f64)),
                    _ => Err(Flow::Err("str.count expects String".into())),
                },
                "join" => match args.first() {
                    Some(Val::List(items)) => {
                        Ok(Val::Str(items.iter().map(Val::show).collect::<Vec<_>>().join(&s)))
                    }
                    _ => Err(Flow::Err("str.join expects List".into())),
                },
                "chars" => Ok(Val::List(s.chars().map(|c| Val::Str(c.to_string())).collect())),
                "isdigit" => Ok(Val::Bool(!s.is_empty() && s.chars().all(|c| c.is_ascii_digit()))),
                "isalpha" => Ok(Val::Bool(!s.is_empty() && s.chars().all(|c| c.is_alphabetic()))),
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
                    if let Some(Val::List(more)) = args.first() {
                        items.extend(more.iter().cloned());
                        Ok(Val::List(items))
                    } else {
                        Err(Flow::Err("list.extend expects List".into()))
                    }
                }
                "pop" => Ok(items.pop().unwrap_or(Val::Null)),
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
                    Ok(Val::Str(items.iter().map(Val::show).collect::<Vec<_>>().join(&sep)))
                }
                "reverse" => {
                    items.reverse();
                    Ok(Val::List(items))
                }
                "sort" => {
                    items.sort_by(|a, b| a.show().cmp(&b.show()));
                    Ok(Val::List(items))
                }
                "len" => Ok(Val::Num(items.len() as f64)),
                _ => Err(Flow::Err(format!("unknown List method '{name}'"))),
            },
            _ => Err(Flow::Err("method call requires object or map/string/list".into())),
        }
    }

    fn call_builtin(&mut self, name: &str, args: &[Val]) -> Result<Option<Val>, Flow> {
        let out = match name {
            "print" | "println" => {
                let s = args.iter().map(Val::show).collect::<Vec<_>>().join(" ");
                println!("{s}");
                Val::Null
            }
            "len" => {
                if args.len() != 1 {
                    return Err(Flow::Err("len expects 1 argument".into()));
                }
                match &args[0] {
                    Val::Str(s) => Val::Num(s.chars().count() as f64),
                    Val::List(v) => Val::Num(v.len() as f64),
                    Val::Map(v) => Val::Num(v.len() as f64),
                    _ => return Err(Flow::Err("len supports String/List/Map".into())),
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
            "__iter_list" => {
                if args.len() != 1 {
                    return Err(Flow::Err("__iter_list expects 1 argument".into()));
                }
                Val::List(self.to_iter(args[0].clone())?)
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
                    let msg = args
                        .get(1)
                        .map(Val::show)
                        .unwrap_or_else(|| "assertion failed".into());
                    return Err(Flow::Err(msg));
                }
                Val::Null
            }
            "str" => {
                if args.len() != 1 {
                    return Err(Flow::Err("str expects 1 argument".into()));
                }
                Val::Str(args[0].show())
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
            "abs" | "sqrt" | "floor" | "ceil" | "round" | "sin" | "cos" | "tan" | "log" | "exp" => {
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
                    "round" => x.round(),
                    "sin" => x.sin(),
                    "cos" => x.cos(),
                    "tan" => x.tan(),
                    "log" => x.ln(),
                    _ => x.exp(),
                };
                Val::Num(r)
            }
            "pow" => match args {
                [Val::Num(a), Val::Num(b)] => Val::Num(a.powf(*b)),
                _ => return Err(Flow::Err("pow expects (Number, Number)".into())),
            },
            "clamp" => match args {
                [Val::Num(x), Val::Num(lo), Val::Num(hi)] => Val::Num(x.clamp(*lo, *hi)),
                _ => return Err(Flow::Err("clamp expects 3 numbers".into())),
            },
            "sum" => match args.first() {
                Some(Val::List(xs)) => {
                    let mut total = 0.0;
                    for x in xs {
                        match x {
                            Val::Num(n) => total += *n,
                            _ => return Err(Flow::Err("sum expects list of numbers".into())),
                        }
                    }
                    Val::Num(total)
                }
                _ => return Err(Flow::Err("sum expects List".into())),
            },
            "min" | "max" => {
                let items = match args {
                    [Val::List(xs)] => xs.as_slice(),
                    many if !many.is_empty() => many,
                    _ => return Err(Flow::Err(format!("{name} expects non-empty List or args"))),
                };
                let mut best: Option<f64> = None;
                for item in items {
                    let Val::Num(n) = item else {
                        return Err(Flow::Err(format!("{name} expects numbers")));
                    };
                    best = Some(match best {
                        None => *n,
                        Some(b) if name == "min" => b.min(*n),
                        Some(b) => b.max(*n),
                    });
                }
                Val::Num(best.ok_or_else(|| Flow::Err(format!("{name} empty")))?)
            }
            "all" => match args.first() {
                Some(Val::List(xs)) => Val::Bool(xs.iter().all(Val::truthy)),
                _ => return Err(Flow::Err("all expects List".into())),
            },
            "any" => match args.first() {
                Some(Val::List(xs)) => Val::Bool(xs.iter().any(Val::truthy)),
                _ => return Err(Flow::Err("any expects List".into())),
            },
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
                Some(Val::List(xs)) => {
                    let mut out = xs.clone();
                    out.reverse();
                    Val::List(out)
                }
                _ => return Err(Flow::Err("reverse expects List".into())),
            },
            "sort" => match args.first() {
                Some(Val::List(xs)) => {
                    let mut out = xs.clone();
                    out.sort_by(|a, b| match (a, b) {
                        (Val::Num(x), Val::Num(y)) => {
                            x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal)
                        }
                        _ => a.show().cmp(&b.show()),
                    });
                    Val::List(out)
                }
                _ => return Err(Flow::Err("sort expects List".into())),
            },
            "unique" => match args.first() {
                Some(Val::List(xs)) => {
                    let mut out = Vec::new();
                    for x in xs {
                        if !out.contains(x) {
                            out.push(x.clone());
                        }
                    }
                    Val::List(out)
                }
                _ => return Err(Flow::Err("unique expects List".into())),
            },
            "enumerate" => match args.first() {
                Some(Val::List(xs)) => Val::List(
                    xs.iter()
                        .enumerate()
                        .map(|(i, v)| Val::List(vec![Val::Num(i as f64), v.clone()]))
                        .collect(),
                ),
                _ => return Err(Flow::Err("enumerate expects List".into())),
            },
            "zip" => match args {
                [Val::List(a), Val::List(b)] => Val::List(
                    a.iter()
                        .zip(b.iter())
                        .map(|(x, y)| Val::List(vec![x.clone(), y.clone()]))
                        .collect(),
                ),
                _ => return Err(Flow::Err("zip expects (List, List)".into())),
            },
            "map_call" => match args {
                [fval, Val::List(xs)] => {
                    let mut out = Vec::with_capacity(xs.len());
                    for x in xs {
                        out.push(self.call_val(fval, vec![x.clone()])?);
                    }
                    Val::List(out)
                }
                _ => return Err(Flow::Err("map_call expects (callable, List)".into())),
            },
            "filter_call" => match args {
                [fval, Val::List(xs)] => {
                    let mut out = Vec::new();
                    for x in xs {
                        if self.call_val(fval, vec![x.clone()])?.truthy() {
                            out.push(x.clone());
                        }
                    }
                    Val::List(out)
                }
                _ => return Err(Flow::Err("filter_call expects (callable, List)".into())),
            },
            "reduce_call" => match args {
                [fval, Val::List(xs), init] => {
                    let mut acc = init.clone();
                    for x in xs {
                        acc = self.call_val(fval, vec![acc, x.clone()])?;
                    }
                    acc
                }
                _ => return Err(Flow::Err("reduce_call expects (callable, List, init)".into())),
            },
            "list" | "tuple" => match args.first() {
                None => Val::List(vec![]),
                Some(Val::List(xs)) => Val::List(xs.clone()),
                Some(Val::Str(s)) => {
                    Val::List(s.chars().map(|c| Val::Str(c.to_string())).collect())
                }
                _ => return Err(Flow::Err("list expects 0 or 1 List/String".into())),
            },
            "set" => match args.first() {
                None => Val::List(vec![]),
                Some(Val::List(xs)) => {
                    let mut out = Vec::new();
                    for x in xs {
                        if !out.contains(x) {
                            out.push(x.clone());
                        }
                    }
                    Val::List(out)
                }
                _ => return Err(Flow::Err("set expects 0 or 1 List".into())),
            },
            "dict" => Val::Map(vec![]),
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
            "is_instance" => match args {
                [Val::Object(id), Val::Str(cls)] => Val::Bool(
                    self.objects
                        .get(*id)
                        .map(|o| self.class_is_a(&o.class_name, cls))
                        .unwrap_or(false),
                ),
                _ => return Err(Flow::Err("is_instance expects (Object, String)".into())),
            },
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
                    std::fs::write(p, content.show()).map_err(|e| Flow::Err(e.to_string()))?;
                    Val::Null
                }
                _ => return Err(Flow::Err("write_file expects (String path, content)".into())),
            },
            "append_file" => match args {
                [Val::Str(p), content] => {
                    use std::io::Write;
                    let mut f = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(p)
                        .map_err(|e| Flow::Err(e.to_string()))?;
                    f.write_all(content.show().as_bytes())
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
            _ => return Ok(None),
        };
        Ok(Some(out))
    }
}

"#;
