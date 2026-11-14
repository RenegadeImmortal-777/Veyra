// CLI tooling commands: debug trace, AST and symbol dumps, lint, bench,
// profile and doc generation.
//
// Extracted from `main.rs` without behavior change; see docs/ENGINE-SPLIT.md.

use std::fs;
use std::path::Path;
use std::time::Instant;

use crate::ast::Stmt;
use crate::lexer::lex;
use crate::parser::Parser;
use crate::pipeline::{compile_source, load_source};
use crate::vm::Vm;

pub(crate) fn debug_file(path: &Path) -> Result<(), String> {
    let src = load_source(path.to_string_lossy().as_ref())?;
    let code = compile_source(&src)?;
    let mut vm = Vm::new(code);
    vm.trace = true;
    vm.run()
}

pub(crate) fn ast_file(path: &Path) -> Result<(), String> {
    let src = load_source(path.to_string_lossy().as_ref())?;
    let ast = Parser::new(lex(&src)?).program()?;
    println!("{ast:#?}");
    Ok(())
}

pub(crate) fn symbols_file(path: &Path) -> Result<(), String> {
    let src = load_source(path.to_string_lossy().as_ref())?;
    let ast = Parser::new(lex(&src)?).program()?;
    println!("Symbols for {}", path.display());
    for stmt in ast {
        match stmt {
            Stmt::Let(name, ty, _, is_const) => {
                println!(
                    "  {} {}{}",
                    if is_const { "const" } else { "let" },
                    name,
                    ty.map(|t| format!(": {t}")).unwrap_or_default()
                );
            }
            Stmt::Fn(name, params, ret, _) | Stmt::AsyncFn(name, params, ret, _) => {
                let ps = params
                    .iter()
                    .map(|(n, t)| match t {
                        Some(t) => format!("{n}: {t}"),
                        None => n.clone(),
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                println!(
                    "  fn {name}({ps}){}",
                    ret.map(|r| format!(" -> {r}")).unwrap_or_default()
                );
            }
            Stmt::DecoratedFn(decorators, name, params, ret, _) => {
                let ps = params
                    .iter()
                    .map(|(n, t)| match t {
                        Some(t) => format!("{n}: {t}"),
                        None => n.clone(),
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                println!(
                    "  @({} decorators) fn {name}({ps}){}",
                    decorators.len(),
                    ret.map(|r| format!(" -> {r}")).unwrap_or_default()
                );
            }
            Stmt::Class(name, bases, _, _, body) => {
                println!(
                    "  class {name}{}",
                    if bases.is_empty() {
                        String::new()
                    } else {
                        format!(" ({} bases)", bases.len())
                    }
                );
                for item in body {
                    match item {
                        Stmt::Fn(method, params, ret, _)
                        | Stmt::AsyncFn(method, params, ret, _)
                        | Stmt::DecoratedFn(_, method, params, ret, _) => {
                            let ps = params
                                .iter()
                                .map(|(n, t)| match t {
                                    Some(t) => format!("{n}: {t}"),
                                    None => n.clone(),
                                })
                                .collect::<Vec<_>>()
                                .join(", ");
                            println!(
                                "    method {method}({ps}){}",
                                ret.map(|r| format!(" -> {r}")).unwrap_or_default()
                            );
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

pub(crate) fn lint_file(path: &Path) -> Result<(), String> {
    let src = fs::read_to_string(path).map_err(|e| e.to_string())?;
    let loaded = load_source(path.to_string_lossy().as_ref())?;
    compile_source(&loaded)?;
    let mut issues = vec![];
    for (i, line) in src.lines().enumerate() {
        if line.contains('\t') {
            issues.push(format!("{}:{}: tab character", path.display(), i + 1));
        }
        if line.len() > 120 {
            issues.push(format!(
                "{}:{}: line exceeds 120 characters",
                path.display(),
                i + 1
            ));
        }
        if line.trim_end() != line {
            issues.push(format!("{}:{}: trailing whitespace", path.display(), i + 1));
        }
    }
    if issues.is_empty() {
        println!("{}: lint OK", path.display());
        Ok(())
    } else {
        for issue in &issues {
            println!("{issue}");
        }
        Err(format!("{} lint issue(s)", issues.len()))
    }
}

pub(crate) fn bench_file(path: &Path, iterations: usize) -> Result<(), String> {
    let src = load_source(path.to_string_lossy().as_ref())?;
    let code = compile_source(&src)?;
    let n = iterations.max(1);
    let mut times = Vec::with_capacity(n);
    for _ in 0..n {
        let start = Instant::now();
        Vm::new(code.clone()).run()?;
        times.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    let best = times.iter().copied().fold(f64::INFINITY, f64::min);
    let mean = times.iter().sum::<f64>() / times.len() as f64;
    println!(
        "{}: {n} run(s) | best={best:.3}ms | mean={mean:.3}ms | bytecode_ops={}",
        path.display(),
        code.len()
    );
    Ok(())
}

pub(crate) fn profile_file(path: &Path) -> Result<(), String> {
    let src = load_source(path.to_string_lossy().as_ref())?;
    let start_compile = Instant::now();
    let code = compile_source(&src)?;
    let compile_ms = start_compile.elapsed().as_secs_f64() * 1000.0;
    let start_run = Instant::now();
    Vm::new(code.clone()).run()?;
    let run_ms = start_run.elapsed().as_secs_f64() * 1000.0;
    println!("profile {}", path.display());
    println!("  source_bytes: {}", src.len());
    println!("  bytecode_ops: {}", code.len());
    println!("  compile_ms: {compile_ms:.3}");
    println!("  run_ms: {run_ms:.3}");
    Ok(())
}

pub(crate) fn doc_file(path: &Path) -> Result<(), String> {
    let src = fs::read_to_string(path).map_err(|e| e.to_string())?;
    let ast = Parser::new(lex(&src)?).program()?;
    let mut out = format!(
        "# API — {}\n\n",
        path.file_name()
            .and_then(|x| x.to_str())
            .unwrap_or("module")
    );
    for stmt in ast {
        match stmt {
            Stmt::Fn(name, params, return_type, _)
            | Stmt::AsyncFn(name, params, return_type, _) => {
                let rendered = params
                    .iter()
                    .map(|(n, t)| match t {
                        Some(t) => format!("{n}: {t}"),
                        None => n.clone(),
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let ret = return_type.unwrap_or_else(|| "Any".into());
                out.push_str(&format!("## fn {name}({rendered}) -> {ret}\n\n"));
            }
            Stmt::DecoratedFn(_, name, params, return_type, _) => {
                let rendered = params
                    .iter()
                    .map(|(n, t)| match t {
                        Some(t) => format!("{n}: {t}"),
                        None => n.clone(),
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let ret = return_type.unwrap_or_else(|| "Any".into());
                out.push_str(&format!("## @decorated fn {name}({rendered}) -> {ret}\n\n"));
            }
            Stmt::Class(name, bases, _, _, body) => {
                if bases.is_empty() {
                    out.push_str(&format!("## class {name}\n\n"));
                } else {
                    out.push_str(&format!("## class {name} ({} bases)\n\n", bases.len()));
                }
                for item in body {
                    match item {
                        Stmt::Fn(method, params, return_type, _)
                        | Stmt::AsyncFn(method, params, return_type, _)
                        | Stmt::DecoratedFn(_, method, params, return_type, _) => {
                            let rendered = params
                                .iter()
                                .map(|(n, t)| match t {
                                    Some(t) => format!("{n}: {t}"),
                                    None => n.clone(),
                                })
                                .collect::<Vec<_>>()
                                .join(", ");
                            let ret = return_type.unwrap_or_else(|| "Any".into());
                            out.push_str(&format!("- `{method}({rendered}) -> {ret}`\n"));
                        }
                        _ => {}
                    }
                }
                out.push('\n');
            }
            _ => {}
        }
    }
    let target = path.with_extension("api.md");
    fs::write(&target, out).map_err(|e| e.to_string())?;
    println!("Generated {}", target.display());
    Ok(())
}
