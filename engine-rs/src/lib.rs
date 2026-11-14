#![allow(
    clippy::collapsible_if,
    clippy::collapsible_match,
    clippy::needless_range_loop,
    clippy::type_complexity,
    clippy::let_and_return,
    clippy::only_used_in_recursion,
    clippy::manual_is_multiple_of
)]

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::ExitCode,
};

#[cfg(test)]
use crate::value::Val;
#[cfg(test)]
use serde_json::Value as JsonValue;

pub const VERSION: &str = "12.0.0-sovereign";

mod aot;
mod ast;
mod bigint;
mod builtins;
mod bytecode;
mod cli;
mod compiler;
mod dap;
mod gpu;
mod ir;
mod jit;
mod lexer;
mod lsp;
mod native_codegen;
mod parser;
mod pipeline;
mod plugin;
mod project;
mod py_bridge;
mod registry;
mod runtime;
mod server;
mod sim_actor;
mod spirv;
mod tensor;
mod tooling;
mod value;
mod vm;

#[cfg(test)]
use crate::bytecode::Op;
use crate::cli::{doctor, new_project, repl, usage};
use crate::dap::dap_server;
#[cfg(test)]
use crate::lexer::lex;
use crate::lsp::lsp_server;
#[cfg(test)]
use crate::lsp::{lsp_definition, lsp_diagnostics, lsp_references, lsp_rename};
use crate::pipeline::compile_source;
#[cfg(test)]
use crate::pipeline::compile_source_unoptimized;
pub use crate::pipeline::{compile_to_vybc, disasm_source, load_source, run_source, run_vybc};
use crate::project::{build_project, fmt_source, project_entry, project_packages, test_project};
use crate::registry::{
    pkg_add, pkg_audit, pkg_build_plugin, pkg_info, pkg_install, pkg_keygen, pkg_list, pkg_pack,
    pkg_publish, pkg_py_bridge, pkg_remove, pkg_search, pkg_sign, pkg_tree, pkg_unpack, pkg_update,
    pkg_verify,
};
#[cfg(test)]
use crate::runtime::export_function_object;
#[cfg(test)]
use crate::server::resolve_static_path;
use crate::server::{serve_directory, serve_registry, serve_routes, serve_veyra_app};
use crate::tooling::{
    ast_file, bench_file, debug_file, doc_file, lint_file, profile_file, symbols_file,
};
#[cfg(test)]
use crate::vm::Vm;

const BUNDLE_MAGIC: &[u8; 8] = b"VEYRABC1";

pub fn extract_embedded_vybc(bytes: &[u8]) -> Option<String> {
    if bytes.len() < 16 {
        return None;
    }
    let footer_start = bytes.len() - 8;
    if &bytes[footer_start..] != BUNDLE_MAGIC {
        return None;
    }
    let len_bytes: [u8; 8] = bytes[footer_start - 8..footer_start].try_into().ok()?;
    let payload_len = u64::from_le_bytes(len_bytes) as usize;
    if footer_start < 8 + payload_len {
        return None;
    }
    let payload_start = footer_start - 8 - payload_len;
    String::from_utf8(bytes[payload_start..footer_start - 8].to_vec()).ok()
}

pub fn bundle_executable(script_path: &Path, out_path: Option<&Path>) -> Result<PathBuf, String> {
    let source = load_source(script_path.to_string_lossy().as_ref())?;
    let vybc = compile_to_vybc(&source)?;
    let exe_path = env::current_exe().map_err(|e| e.to_string())?;
    let mut base_bytes = fs::read(&exe_path).map_err(|e| e.to_string())?;
    if base_bytes.len() >= 16 && &base_bytes[base_bytes.len() - 8..] == BUNDLE_MAGIC {
        let f = base_bytes.len() - 8;
        if let Ok(lb) = <[u8; 8]>::try_from(&base_bytes[f - 8..f]) {
            let plen = u64::from_le_bytes(lb) as usize;
            if f >= 8 + plen {
                base_bytes.truncate(f - 8 - plen);
            }
        }
    }
    let payload = vybc.as_bytes();
    base_bytes.extend_from_slice(payload);
    base_bytes.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    base_bytes.extend_from_slice(BUNDLE_MAGIC);
    let target = match out_path {
        Some(p) => p.to_path_buf(),
        None => {
            let stem = script_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("veyra-app");
            if cfg!(windows) {
                script_path.with_file_name(format!("{stem}.exe"))
            } else {
                script_path.with_file_name(stem)
            }
        }
    };
    fs::write(&target, &base_bytes).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(&target) {
            let mut perms = meta.permissions();
            perms.set_mode(0o755);
            let _ = fs::set_permissions(&target, perms);
        }
    }
    Ok(target)
}

pub fn run_cli() -> ExitCode {
    if let Ok(exe) = env::current_exe() {
        if let Ok(bytes) = fs::read(&exe) {
            if let Some(embedded_vybc) = extract_embedded_vybc(&bytes) {
                return match run_vybc(&embedded_vybc) {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(e) => {
                        eprintln!("veyra: {e}");
                        ExitCode::FAILURE
                    }
                };
            }
        }
    }
    let a: Vec<String> = env::args().collect();
    let r: Result<(), String> = match a.get(1).map(String::as_str) {
        Some("--version") | Some("version") => {
            println!("Veyra {VERSION}");
            Ok(())
        }
        Some("bundle") => {
            let script = a
                .get(2)
                .ok_or("usage: veyra bundle <script.vy> [output]".to_string());
            script.and_then(|s| {
                let out = a.get(3).map(Path::new);
                let built = bundle_executable(Path::new(s), out)?;
                println!("Bundled standalone executable: {}", built.display());
                Ok(())
            })
        }
        Some("doctor") => {
            doctor();
            Ok(())
        }
        Some("repl") | None => repl(),
        Some("new") => a
            .get(2)
            .ok_or("missing project name".into())
            .and_then(|n| new_project(n)),
        Some("pkg") => match a.get(2).map(String::as_str) {
            Some("list") => pkg_list(),
            Some("installed") => project_packages(),
            Some("install") => pkg_install(),
            Some("publish") => pkg_publish(),
            Some("tree") => pkg_tree(),
            Some("info") => a
                .get(3)
                .ok_or("missing package name".into())
                .and_then(|n| pkg_info(n)),
            Some("update") => pkg_update(a.get(3).map(String::as_str)),
            Some("keygen") => pkg_keygen(a.get(3).map(String::as_str)),
            Some("sign") => pkg_sign(
                a.get(3).map(Path::new),
                a.get(4).map(String::as_str),
            ),
            Some("verify") => pkg_verify(a.get(3).map(String::as_str)),
            Some("pack") => pkg_pack(a.get(3).map(Path::new), a.get(4).map(Path::new)),
            Some("unpack") => a
                .get(3)
                .ok_or("missing bundle path".into())
                .and_then(|p| pkg_unpack(Path::new(p), a.get(4).map(Path::new))),
            Some("build-plugin") => pkg_build_plugin(a.get(3).map(Path::new)),
            Some("py-bridge") => {
                let mut pip_install = false;
                let mut positional = Vec::new();
                for arg in a.iter().skip(3) {
                    if arg == "--pip-install" {
                        pip_install = true;
                    } else {
                        positional.push(arg.as_str());
                    }
                }
                let out_dir = positional.get(1).copied().map(Path::new);
                positional
                    .first()
                    .copied()
                    .ok_or_else(|| "missing Python/PyPI package name".to_string())
                    .and_then(|pypi_name| pkg_py_bridge(pypi_name, out_dir, pip_install))
            }
            Some("serve") => {
                let dir = a
                    .get(3)
                    .map(PathBuf::from)
                    .unwrap_or_else(crate::project::veyra_home);
                let port = a.get(4).and_then(|x| x.parse::<u16>().ok()).unwrap_or(8787);
                serve_registry(&dir, port)
            }
            Some("search") => a
                .get(3)
                .ok_or("missing search term".into())
                .and_then(|q| pkg_search(q)),
            Some("add") => a
                .get(3)
                .ok_or("missing package name".into())
                .and_then(|n| pkg_add(n)),
            Some("remove") => a
                .get(3)
                .ok_or("missing package name".into())
                .and_then(|n| pkg_remove(n)),
            _ => Err(
                "usage: veyra pkg <list|search|info|tree|installed|install|update|publish|keygen|sign|verify|pack|unpack|build-plugin|py-bridge|serve|add|remove> [args]"
                    .into(),
            ),
        },
        Some("add") => a
            .get(2)
            .ok_or("missing package name".into())
            .and_then(|n| pkg_add(n)),
        Some("remove") => a
            .get(2)
            .ok_or("missing package name".into())
            .and_then(|n| pkg_remove(n)),
        Some("install") => pkg_install(),
        Some("publish") => pkg_publish(),
        Some("packages") => project_packages(),
        Some("registry") => pkg_list(),
        Some("check") => {
            let path = match a.get(2) {
                Some(p) => Ok(PathBuf::from(p)),
                None => project_entry(),
            };
            path.and_then(|p| load_source(p.to_string_lossy().as_ref()))
                .and_then(|s| compile_source(&s).map(|_| println!("OK")))
        }
        Some("run") => (|| -> Result<(), String> {
            let mut caps_arg: Option<Vec<String>> = None;
            let mut file_arg: Option<PathBuf> = None;
            let mut idx = 2usize;
            while idx < a.len() {
                if a[idx] == "--caps" {
                    let raw = a.get(idx + 1).ok_or("missing capability list after --caps")?;
                    caps_arg = Some(
                        raw.split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect(),
                    );
                    idx += 2;
                } else if let Some(raw) = a[idx].strip_prefix("--caps=") {
                    caps_arg = Some(
                        raw.split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect(),
                    );
                    idx += 1;
                } else if file_arg.is_none() {
                    file_arg = Some(PathBuf::from(&a[idx]));
                    idx += 1;
                } else {
                    return Err("usage: veyra run [--caps cap1,cap2] [file.vy]".into());
                }
            }
            let p = match file_arg {
                Some(p) => p,
                None => project_entry()?,
            };
            let manifest_caps: Option<Vec<String>> = caps_arg.or_else(|| {
                let cwd = env::current_dir().ok()?;
                let root = crate::project::project_root(&cwd).ok()?;
                let txt = fs::read_to_string(root.join("veyra.json")).ok()?;
                let j: serde_json::Value = serde_json::from_str(&txt).ok()?;
                let arr = j.get("capabilities")?.as_array()?;
                Some(
                    arr.iter()
                        .filter_map(|v| v.as_str().map(ToString::to_string))
                        .collect(),
                )
            });
            let exec = || {
                if p.extension().and_then(|e| e.to_str()) == Some("vybc") {
                    let bc = fs::read_to_string(&p).map_err(|e| e.to_string())?;
                    run_vybc(&bc)
                } else {
                    load_source(p.to_string_lossy().as_ref()).and_then(|s| run_source(&s))
                }
            };
            if let Some(caps) = manifest_caps {
                crate::runtime::run_with_caps(caps, exec)
            } else {
                exec()
            }
        })(),
        Some("lint") => {
            let path = match a.get(2) {
                Some(p) => Ok(PathBuf::from(p)),
                None => project_entry(),
            };
            path.and_then(|p| lint_file(&p))
        }
        Some("debug") => {
            let path = match a.get(2) {
                Some(p) => Ok(PathBuf::from(p)),
                None => project_entry(),
            };
            path.and_then(|p| debug_file(&p))
        }
        Some("ast") => {
            let path = match a.get(2) {
                Some(p) => Ok(PathBuf::from(p)),
                None => project_entry(),
            };
            path.and_then(|p| ast_file(&p))
        }
        Some("symbols") => {
            let path = match a.get(2) {
                Some(p) => Ok(PathBuf::from(p)),
                None => project_entry(),
            };
            path.and_then(|p| symbols_file(&p))
        }
        Some("lsp") => lsp_server(),
        Some("dap") => dap_server(),
        Some("test") => test_project(),
        Some("bench") => {
            let path = match a.get(2) {
                Some(p) => Ok(PathBuf::from(p)),
                None => project_entry(),
            };
            let iterations = a.get(3).and_then(|x| x.parse::<usize>().ok()).unwrap_or(10);
            path.and_then(|p| bench_file(&p, iterations))
        }
        Some("profile") => {
            let path = match a.get(2) {
                Some(p) => Ok(PathBuf::from(p)),
                None => project_entry(),
            };
            path.and_then(|p| profile_file(&p))
        }
        Some("doc") => {
            let path = match a.get(2) {
                Some(p) => Ok(PathBuf::from(p)),
                None => project_entry(),
            };
            path.and_then(|p| doc_file(&p))
        }
        Some("aot") => (|| -> Result<(), String> {
            let mut input_arg: Option<PathBuf> = None;
            let mut output_arg: Option<PathBuf> = None;
            let mut direct = false;
            let mut direct_fmt: Option<crate::native_codegen::DirectNativeFormat> = None;
            let mut idx = 2usize;
            while idx < a.len() {
                if a[idx] == "--direct" {
                    direct = true;
                    idx += 1;
                } else if a[idx] == "--elf" || a[idx] == "--target=elf64" {
                    direct = true;
                    direct_fmt = Some(crate::native_codegen::DirectNativeFormat::Elf64);
                    idx += 1;
                } else if a[idx] == "--pe" || a[idx] == "--target=pe64" {
                    direct = true;
                    direct_fmt = Some(crate::native_codegen::DirectNativeFormat::Pe64);
                    idx += 1;
                } else if a[idx] == "-o" || a[idx] == "--out" {
                    let out = a.get(idx + 1).ok_or("missing output path after -o")?;
                    output_arg = Some(PathBuf::from(out));
                    idx += 2;
                } else if input_arg.is_none() {
                    input_arg = Some(PathBuf::from(&a[idx]));
                    idx += 1;
                } else if output_arg.is_none() {
                    output_arg = Some(PathBuf::from(&a[idx]));
                    idx += 1;
                } else {
                    return Err("usage: veyra aot [--direct] [--elf|--pe] [file.vy] [out_exe]".into());
                }
            }
            let input = match input_arg {
                Some(p) => p,
                None => project_entry()?,
            };
            if direct {
                crate::native_codegen::compile_file_to_direct_native(
                    &input,
                    output_arg.as_deref(),
                    direct_fmt,
                )
                .map(|_| ())
            } else {
                crate::aot::compile_file_to_native(&input, output_arg.as_deref()).map(|_| ())
            }
        })(),
        Some("wasm") => (|| -> Result<(), String> {
            let mut input_arg: Option<PathBuf> = None;
            let mut output_arg: Option<PathBuf> = None;
            let mut run_fn: Option<String> = None;
            let mut run_args: Vec<f64> = Vec::new();
            let mut disasm = false;
            let mut idx = 2usize;
            while idx < a.len() {
                match a[idx].as_str() {
                    "-o" | "--out" => {
                        let out = a.get(idx + 1).ok_or("missing output path after -o")?;
                        output_arg = Some(PathBuf::from(out));
                        idx += 2;
                    }
                    "--disasm" => {
                        disasm = true;
                        idx += 1;
                    }
                    "--run" => {
                        let fn_name = a.get(idx + 1).cloned().unwrap_or_else(|| "main".into());
                        run_fn = Some(fn_name);
                        idx += 2;
                        while idx < a.len() && !a[idx].starts_with('-') {
                            let val = a[idx].parse::<f64>().map_err(|_| {
                                format!("invalid numeric wasm argument '{}'", a[idx])
                            })?;
                            run_args.push(val);
                            idx += 1;
                        }
                    }
                    _ if input_arg.is_none() => {
                        input_arg = Some(PathBuf::from(&a[idx]));
                        idx += 1;
                    }
                    _ => return Err("usage: veyra wasm <file.vy> [-o out.wasm] [--disasm] [--run <fn> [args...]]".into()),
                }
            }
            let input = match input_arg {
                Some(p) => p,
                None => project_entry()?,
            };
            if !disasm
                && run_fn.is_none()
                && input.extension().and_then(|e| e.to_str()) != Some("wasm")
            {
                let out = output_arg.unwrap_or_else(|| input.with_extension("wasm"));
                return crate::native_codegen::compile_file_to_wasm(&input, &out);
            }
            let bytes = if input.extension().and_then(|e| e.to_str()) == Some("wasm") {
                fs::read(&input).map_err(|e| e.to_string())?
            } else {
                let src = load_source(input.to_string_lossy().as_ref())?;
                crate::native_codegen::compile_source_to_wasm(&src)?
            };
            if let Some(out) = output_arg {
                if let Some(parent) = out.parent()
                    && !parent.as_os_str().is_empty()
                {
                    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                fs::write(&out, &bytes).map_err(|e| e.to_string())?;
            } else if !disasm && run_fn.is_none() {
                let out = input.with_extension("wasm");
                fs::write(&out, &bytes).map_err(|e| e.to_string())?;
            }
            if disasm {
                println!("{}", crate::native_codegen::wasm_disasm(&bytes)?);
            }
            if let Some(fn_name) = run_fn {
                let (res, _) = crate::native_codegen::wasm_run(&bytes, &fn_name, &run_args)?;
                if res.fract() == 0.0 && res.abs() < 9.0e15 {
                    println!("{}", res as i64);
                } else {
                    println!("{res}");
                }
            }
            Ok(())
        })(),
        Some("build") => build_project(a.iter().skip(2).any(|x| x == "--native")),
        Some("audit") => pkg_audit(),
        Some("serve") => {
            let dir = a
                .get(2)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            let port = a.get(3).and_then(|x| x.parse::<u16>().ok()).unwrap_or(8080);
            serve_directory(&dir, port)
        }
        Some("notebook") => {
            let dir = a
                .get(2)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            let port = a.get(3).and_then(|x| x.parse::<u16>().ok()).unwrap_or(8787);
            crate::server::serve_notebook(&dir, port)
        }
        Some("ffi") => (|| -> Result<(), String> {
            match a.get(2).map(String::as_str) {
                Some("bindgen") => {
                    let header_path = a.get(3).ok_or("usage: veyra ffi bindgen <header.h> <lib_path> [-o out.vy]")?;
                    let lib_path = a.get(4).ok_or("usage: veyra ffi bindgen <header.h> <lib_path> [-o out.vy]")?;
                    let out_path = if a.get(5).map(String::as_str) == Some("-o") {
                        Some(a.get(6).ok_or("missing path after -o")?.clone())
                    } else {
                        None
                    };
                    let hdr = fs::read_to_string(header_path).map_err(|e| e.to_string())?;
                    let generated = crate::plugin::ffi_bindgen(&hdr, lib_path)?;
                    if let Some(out) = out_path {
                        fs::write(&out, generated).map_err(|e| e.to_string())?;
                        println!("wrote {out}");
                    } else {
                        print!("{generated}");
                    }
                    Ok(())
                }
                _ => Err("usage: veyra ffi bindgen <header.h> <lib_path> [-o out.vy]".into()),
            }
        })(),
        Some("web") => a.get(2).ok_or("missing routes.json".into()).and_then(|p| {
            let port = a.get(3).and_then(|x| x.parse::<u16>().ok()).unwrap_or(8080);
            serve_routes(Path::new(p), port)
        }),
        Some("app") => {
            let app = a.get(2).ok_or("missing app.vy".into());
            let routes = a.get(3).ok_or("missing routes.json".into());
            match (app, routes) {
                (Ok(app), Ok(routes)) => {
                    let port = a.get(4).and_then(|x| x.parse::<u16>().ok()).unwrap_or(8080);
                    serve_veyra_app(Path::new(app), Path::new(routes), port)
                }
                (Err(e), _) | (_, Err(e)) => Err(e),
            }
        }
        Some("disasm") => {
            let path = match a.get(2) {
                Some(p) => Ok(PathBuf::from(p)),
                None => project_entry(),
            };
            path.and_then(|p| load_source(p.to_string_lossy().as_ref()))
                .and_then(|s| disasm_source(&s))
        }
        Some("fmt") => a.get(2).ok_or("missing file".into()).and_then(|p| {
            let s = fs::read_to_string(p).map_err(|e| e.to_string())?;
            fs::write(p, fmt_source(&s)).map_err(|e| e.to_string())
        }),
        Some(x) if x.ends_with(".vy") => fs::read_to_string(x)
            .map_err(|e| e.to_string())
            .and_then(|s| run_source(&s)),
        Some(x) if x.ends_with(".vybc") => fs::read_to_string(x)
            .map_err(|e| e.to_string())
            .and_then(|s| run_vybc(&s)),
        _ => {
            usage();
            Ok(())
        }
    };
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("veyra: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lex_math() {
        assert!(lex("let x=1+2;").is_ok());
    }
    #[test]
    fn parse_control() {
        assert!(compile_source("let x=0; while x<3 { x=x+1; }").is_ok());
    }
    #[test]
    fn vm_math() {
        assert!(run_source("let x=2+3*4;").is_ok());
    }
    #[test]
    fn list_builtin() {
        assert!(run_source("let x=[1,2,3]; let n=len(x);").is_ok());
    }
    #[test]
    fn for_range_loop() {
        assert!(run_source("let s=0; for n in range(0,5) { s=s+n; }").is_ok());
    }
    #[test]
    fn user_function() {
        assert!(
            run_source("fn add(a: Int, b: Int) -> Int { return a+b; } let x=add(2,3);").is_ok()
        );
    }
    #[test]
    fn recursive_function() {
        assert!(run_source("fn fib(n: Int) -> Int { if n<=1 { return n; } return fib(n-1)+fib(n-2); } let x=fib(8);").is_ok());
    }
    #[test]
    fn map_builtin() {
        assert!(run_source("let m={\"a\":1,\"b\":2}; let n=len(m); let x=get(m,\"b\");").is_ok());
    }
    #[test]
    fn native_indexing() {
        assert!(
            run_source("let xs=[10,20,30]; let x=xs[1]; let m={\"a\":7}; let y=m[\"a\"];").is_ok()
        );
    }
    #[test]
    fn const_is_enforced() {
        assert!(run_source("const x=1; x=2;").is_err());
    }
    #[test]
    fn boolean_logic() {
        assert!(run_source("let x=true and not false; let y=false or x;").is_ok());
    }
    #[test]
    fn native_json_regex_hash() {
        assert!(run_source(
            "let j=json_parse(\"{\\\"a\\\":7}\"); let x=get(j,\"a\"); let m=regex_findall(\"[0-9]+\",\"a12b34\"); let h=sha256(\"veyra\");"
        ).is_ok());
    }
    #[test]
    fn native_file_io() {
        let p = env::temp_dir().join("veyra_native_io_test.txt");
        let s = format!(
            "write_file(\"{}\",\"hello\"); let x=read_file(\"{}\");",
            p.to_string_lossy().replace('\\', "\\\\"),
            p.to_string_lossy().replace('\\', "\\\\")
        );
        assert!(run_source(&s).is_ok());
        let _ = fs::remove_file(p);
    }
    #[test]
    fn native_science_math() {
        assert!(run_source(
            "let x=[1,2,3,4]; let m=vec_mean(x); let s=vec_std(x); let d=vec_dot([1,2],[3,4]); let z=matmul([[1,2],[3,4]],[[5,6],[7,8]]);"
        ).is_ok());
    }
    #[test]
    fn native_ml() {
        assert!(run_source(
            "let model=linreg_fit([1,2,3,4],[3,5,7,9]); let p=linreg_predict(model,5); let c=knn_predict([[0,0],[1,1],[9,9]],[\"a\",\"a\",\"b\"],[0.5,0.5],2); let centers=kmeans([[0,0],[0,1],[9,9],[9,8]],2,5); let h=dense([1,2],[[1,0],[0,1]],[0,0]); let r=relu([-1,2]); let s=softmax([1,2,3]); let n=normalize([10,20,30]);"
        ).is_ok());
    }
    #[test]
    fn native_csv() {
        let p = env::temp_dir().join("veyra_native_data.csv");
        fs::write(&p, "x,y\n1,2\n3,4\n").unwrap();
        let s = format!(
            "let rows=csv_read(\"{}\"); let x=rows[0][\"x\"];",
            p.to_string_lossy().replace('\\', "\\\\")
        );
        assert!(run_source(&s).is_ok());
        let _ = fs::remove_file(p);
    }
    #[test]
    fn native_table_plot() {
        let p = env::temp_dir().join("veyra_native_plot.svg");
        let s = format!(
            "let rows=[{{\"x\":1,\"g\":\"a\"}},{{\"x\":3,\"g\":\"b\"}}]; let c=table_column(rows,\"x\"); let d=table_describe(rows,\"x\"); let f=table_filter_eq(rows,\"g\",\"a\"); plot_svg(\"{}\",[1,2,3],[2,4,3]);",
            p.to_string_lossy().replace('\\', "\\\\")
        );
        assert!(run_source(&s).is_ok());
        assert!(p.exists());
        let _ = fs::remove_file(p);
    }
    #[test]
    fn general_iteration() {
        assert!(run_source("let total=0; for x in [1,2,3,4] { total=total+x; }").is_ok());
        assert!(run_source("let out=\"\"; for ch in \"abc\" { out=out+ch; }").is_ok());
    }
    #[test]
    fn explicit_exceptions() {
        assert!(
            run_source(
                "let caught=false; try { throw \"boom\"; } catch err { caught = err == \"boom\"; }"
            )
            .is_ok()
        );
        assert!(run_source("throw \"uncaught\";").is_err());
    }
    #[test]
    fn lexical_capture() {
        assert!(run_source("fn outer(x: Int) -> Int { let y=3; fn add(z: Int) -> Int { return y+z; } return add(x); } let result=outer(4);").is_ok());
    }
    #[test]
    fn native_tasks() {
        assert!(run_source("fn square(n: Int) -> Int { return n*n; } let task=spawn(\"square\",[9]); let result=await_task(task);").is_ok());
    }
    #[test]
    fn native_ffi_i32() {
        if cfg!(target_os = "windows") {
            assert!(run_source("let ok=ffi_available(\"msvcrt.dll\"); let x=ffi_i32(\"msvcrt.dll\",\"abs\",[-7]);").is_ok());
        }
    }
    #[test]
    fn native_classes_inheritance() {
        let src = "class Animal { fn init(self, name: String) -> Null { self.name = name; return null; } fn speak(self) -> String { return self.name; } } class Dog: Animal { fn rename(self, name: String) -> String { self.name = name; return self.name; } } let d=Dog(\"Rex\"); let a=d.speak(); let b=d.rename(\"Max\"); let c=d.name;";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn loop_control() {
        assert!(
            run_source(
                "let s=0; for n in range(0,10) { if n==2 { continue; } if n==5 { break; } s=s+n; }"
            )
            .is_ok()
        );
        assert!(compile_source("break;").is_err());
        assert!(compile_source("continue;").is_err());
    }
    #[test]
    fn runtime_type_enforcement() {
        assert!(run_source("let x: Int=1; x=2;").is_ok());
        assert!(run_source("let x: Int=1; x=\"bad\";").is_err());
        assert!(run_source("let x: Int=\"bad\";").is_err());
        assert!(run_source("fn f(x: Int)->Int{return x+1;} let y=f(3);").is_ok());
        assert!(run_source("fn f(x: Int)->Int{return x+1;} let y=f(\"bad\");").is_err());
        assert!(run_source("fn f()->Int{return \"bad\";} let y=f();").is_err());
        assert!(
            run_source(
                "class A {} class B: A {} fn use_a(x: A)->A{return x;} let b=B(); let x=use_a(b);"
            )
            .is_ok()
        );
        assert!(
            run_source(
                "class A {} class C {} fn use_a(x: A)->A{return x;} let c=C(); let x=use_a(c);"
            )
            .is_err()
        );
    }
    #[test]
    fn functional_collections() {
        let src = "fn twice(x: Int)->Int{return x*2;} fn even(x: Int)->Bool{return x%2==0;} fn add(a: Int,b: Int)->Int{return a+b;} let a=append([1,2],3); let b=slice(a,1,3); let c=reverse(b); let d=sort([3,1,2]); let e=unique([1,1,2,2,3]); let f=enumerate([7,8]); let g=zip([1,2],[3,4]); let h=map_call(\"twice\",[1,2,3]); let i=filter_call(\"even\",[1,2,3,4]); let j=reduce_call(\"add\",[1,2,3],0);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn everyday_builtins() {
        let src = "let a=int(\"7\"); let b=float(\"2.5\"); let c=bool(1); assert(a==7); let s=sum([1,2,3]); let lo=min([4,2,9]); let hi=max([4,2,9]); let aa=all([true,1,\"x\"]); let an=any([false,0,3]); let t=tuple([1,2]); let st=set([1,1,2]); let has=contains(st,2); let ix=index_of([9,8,7],8);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn expanded_science_data_ml() {
        let src = "let med=vec_median([1,2,9,4]); let p=vec_percentile([1,2,3,4],75); let c=correlation([1,2,3],[2,4,6]); let rows=[{\"id\":1,\"g\":\"a\"},{\"id\":2,\"g\":\"b\"},{\"id\":3,\"g\":\"a\"}]; let groups=table_group_count(rows,\"g\"); let sorted=table_sort(rows,\"id\"); let selected=table_select(rows,[\"id\"]); let joined=table_inner_join(rows,[{\"rid\":1,\"v\":9}],\"id\",\"rid\"); let model=logreg_fit([[0],[1],[2],[3]],[0,0,1,1],100,0.1); let prob=logreg_predict(model,[3]);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn channels_work() {
        assert!(run_source("let ch=channel(); send(ch,\"hello\"); let x=recv(ch);").is_ok());
    }
    #[test]
    fn await_syntax_and_introspection() {
        let src = "class A {} class B: A {} fn square(n: Int)->Int{return n*n;} let task=spawn(\"square\",[6]); let result=await task; let b=B(); let t=type_of(b); let yes=is_instance(b,\"A\"); let field=has_field(b,\"x\");";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn lambda_function_values() {
        let src = "let offset=4; let add: Function=fn(x: Int)->Int{return x+offset;}; let y=add(6); assert(y==10); assert(type_of(add)==\"Function\");";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn named_function_values() {
        let src = "fn twice(x: Int)->Int{return x*2;} let f: Function=twice; let ys=map_call(f,[1,2,3]); assert(ys[0]==2); assert(ys[2]==6);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn list_comprehensions() {
        let src = "let factor=3; let xs=[x*factor for x in [1,2,3,4] if x%2==0]; assert(len(xs)==2); assert(xs[0]==6); assert(xs[1]==12);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn match_case_statement() {
        let src = "let x=2; let label=\"\"; match x { case 1 { label=\"one\"; } case 2 { label=\"two\"; } case _ { label=\"other\"; } } assert(label==\"two\");";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn advanced_linear_algebra() {
        let src = "let d=matrix_det([[4,7],[2,6]]); assert(abs(d-10)<0.000001); let inv=matrix_inverse([[4,7],[2,6]]); assert(abs(inv[0][0]-0.6)<0.000001); let x=linear_solve([[3,2],[1,2]],[5,5]); assert(abs(x[0])<0.000001); assert(abs(x[1]-2.5)<0.000001);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn generators_yield_and_collect() {
        let src = "fn count_to(n: Int) -> Generator { let i=0; while i<n { yield i; i=i+1; } } let g=count_to(4); let a=next(g); let b=next(g); let rest=collect(g); assert(a==0); assert(b==1); assert(len(rest)==2);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn lsp_navigation_helpers() {
        let src = "fn add(x: Int)->Int{return x+1;} let y=add(4); let z=add(y);";
        let def = lsp_definition(src, "file:///test.vy", "add");
        assert!(!def.is_null());
        let refs = lsp_references(src, "file:///test.vy", "add");
        assert!(refs.as_array().map(|x| x.len()).unwrap_or(0) >= 3);
    }
    #[test]
    fn grouped_data_and_plotting() {
        let scatter = env::temp_dir().join("veyra_scatter.svg");
        let bars = env::temp_dir().join("veyra_bars.svg");
        let src = format!(
            "let rows=[{{\"g\":\"a\",\"x\":1}},{{\"g\":\"a\",\"x\":3}},{{\"g\":\"b\",\"x\":4}}]; let sums=table_group_sum(rows,\"g\",\"x\"); let means=table_group_mean(rows,\"g\",\"x\"); let h=histogram([1,1,2,3,4,4],3); scatter_svg(\"{}\",[1,2,3],[3,2,5]); bar_svg(\"{}\",[\"a\",\"b\"],[4,5]);",
            scatter.to_string_lossy().replace('\\', "\\\\"),
            bars.to_string_lossy().replace('\\', "\\\\")
        );
        assert!(run_source(&src).is_ok());
        assert!(scatter.exists());
        assert!(bars.exists());
        let _ = fs::remove_file(scatter);
        let _ = fs::remove_file(bars);
    }
    #[test]
    fn async_functions_methods_and_gather() {
        let src = "async fn square(n: Int)->Int{sleep_ms(1);return n*n;} class Worker { async fn plus(self,n:Int)->Int{return n+1;} } let a=square(3); let b=square(4); let values=gather([a,b]); assert(values[0]==9); assert(values[1]==16); let w=Worker(); let x=await w.plus(7); assert(x==8);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn context_manager_with_as() {
        let src = "class Resource { fn init(self)->Null{self.closed=false;return null;} fn enter(self)->Resource{return self;} fn exit(self)->Null{self.closed=true;return null;} } with Resource() as r { assert(r.closed==false); } assert(r.closed==true);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn decorator_syntax_identity() {
        let src = "fn identity(f: Function)->Function{return f;} @identity fn plus_one(x:Int)->Int{return x+1;} assert(plus_one(4)==5);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn escaping_closure_objects() {
        let src = "fn twice_result(f:Function)->Function{return fn(x:Int)->Int{return f(x)*2;};} @twice_result fn plus_one(x:Int)->Int{return x+1;} fn make_adder(offset:Int)->Function{return fn(x:Int)->Int{return x+offset;};} let add7=make_adder(7); assert(plus_one(4)==10); assert(add7(5)==12);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn module_map_dispatch() {
        let mut vm = Vm::new(compile_source("fn plus_one(x:Int)->Int{return x+1;}").unwrap());
        vm.run().unwrap();
        let def = vm.funcs.get("plus_one").unwrap().clone();
        let key = export_function_object(def).unwrap();
        let module = Val::Map(vec![(Val::Str("plus".into()), Val::Function(key))]);
        let mut caller = Vm::new(vec![
            Op::Load("module".into()),
            Op::Const(Val::Num(4.0)),
            Op::CallMethod("plus".into(), 1),
            Op::Const(Val::Num(5.0)),
            Op::Eq,
            Op::Call("assert".into(), 1),
            Op::Halt,
        ]);
        caller.vars.insert("module".into(), module);
        assert!(caller.run().is_ok());
    }
    #[test]
    fn ml_metrics_and_split() {
        let src = "let m=classification_metrics([1,0,1,1],[1,0,0,1]); assert(m[\"accuracy\"]==0.75); assert(m[\"precision\"]==1); let s=train_test_split([1,2,3,4],[10,20,30,40],0.25); assert(len(s[\"train_x\"])==3); assert(len(s[\"test_x\"])==1);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn path_and_text_services() {
        let src = "assert(starts_with(\"veyra\",\"vey\")); assert(ends_with(\"veyra\",\"yra\")); assert(replace_text(\"a-b\",\"-\",\"+\")==\"a+b\"); let p=path_join(\"a\",\"b.txt\"); assert(path_name(p)==\"b.txt\"); assert(path_extension(p)==\"txt\");";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn finally_cleanup_semantics() {
        let src = "let cleaned=false; try { throw \"boom\"; } catch err { assert(err==\"boom\"); } finally { cleaned=true; } assert(cleaned); let normal=false; try { normal=true; } catch err {} finally { normal=true; } assert(normal);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn dataframe_extended_helpers() {
        let src = "let rows=[{\"id\":1,\"x\":1},{\"id\":2,\"x\":null},{\"id\":3,\"x\":3}]; let h=table_head(rows,2); let t=table_tail(rows,1); assert(len(h)==2); assert(t[0][\"id\"]==3); let d=table_drop_nulls(rows,\"x\"); assert(len(d)==2); let f=table_fill_null(rows,\"x\",9); assert(f[1][\"x\"]==9); let joined=table_left_join(rows,[{\"rid\":1,\"name\":\"a\"}],\"id\",\"rid\"); assert(len(joined)==3);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn tensor_autograd_and_training() {
        let src = "let x=tensor([[1],[2],[3],[4]],false); let y=tensor([[2],[4],[6],[8]],false); let w=tensor([[0]],true); let epoch=0; while epoch<40 { let pred=tensor_matmul(x,w); let diff=tensor_sub(pred,y); let sq=tensor_mul(diff,diff); let loss=tensor_mean(sq); tensor_backward(loss); tensor_step(w,0.05); tensor_zero_grad(w); epoch=epoch+1; } let trained=tensor_values(w); assert(trained[0]>1.9 and trained[0]<2.1);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn loop_local_declaration_reinitializes() {
        let src = "let i=0; let total=0; while i<3 { let local=i+1; total=total+local; i=i+1; } assert(total==6);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn tensor_extended_autograd_and_shapes() {
        let src = "let x=tensor([1,2],true); let y=tensor_sigmoid(x); let loss=tensor_sum(y); tensor_backward(loss); let g=tensor_grad(x); assert(g[0]>0.19 and g[0]<0.20); assert(g[1]>0.10 and g[1]<0.11); let m=tensor([[1,2,3],[4,5,6]],false); let tr=tensor_transpose(m); let sh=tensor_shape(tr); assert(sh[0]==3 and sh[1]==2); let r=tensor_reshape(tr,[6]); let v=tensor_values(r); assert(v[0]==1 and v[1]==4 and v[5]==6);";
        if let Err(e) = run_source(src) {
            panic!("extended tensor test failed: {e}");
        }
    }
    #[test]
    fn tensor_advanced_ops_and_softmax() {
        let src = "let x=tensor([2,4],true); let y=tensor_pow(x,2); let loss=tensor_sum(y); tensor_backward(loss); let g=tensor_grad(x); assert(g[0]==4 and g[1]==8); let a=tensor([6,8],true); let b=tensor([2,4],false); let q=tensor_div(a,b); let qloss=tensor_sum(q); tensor_backward(qloss); let qg=tensor_grad(a); assert(qg[0]==0.5 and qg[1]==0.25); let n=tensor_neg(a); let nv=tensor_values(n); assert(nv[0]==-6 and nv[1]==-8); let s=tensor_softmax(tensor([[1,2,3],[2,2,2]],false)); let sv=tensor_values(s); assert(sv[2]>sv[1] and sv[1]>sv[0]); assert(sv[3]>0.33 and sv[3]<0.34); assert(tensor_numel(s)==6);";
        if let Err(e) = run_source(src) {
            panic!("advanced tensor ops failed: {e}");
        }
    }
    #[test]
    fn adam_optimizer_trains_parameter() {
        let src = "let target=tensor([3],false); let w=tensor([0],true); let epoch=0; while epoch<120 { let diff=tensor_sub(w,target); let loss=tensor_mean(tensor_pow(diff,2)); tensor_backward(loss); tensor_adam_step(w,0.1,0.9,0.999,0.00000001); tensor_zero_grad(w); epoch=epoch+1; } let value=tensor_values(w); assert(value[0]>2.9 and value[0]<3.1); tensor_reset_optimizer(w);";
        if let Err(e) = run_source(src) {
            panic!("adam optimizer test failed: {e}");
        }
    }
    #[test]
    fn tensor_serialization_round_trip() {
        let path = env::temp_dir().join("veyra_tensor_round_trip.json");
        let escaped = path.to_string_lossy().replace('\\', "\\\\");
        let src = format!(
            "let x=tensor([[1,2],[3,4]],true); tensor_save(x,\"{escaped}\"); let y=tensor_load(\"{escaped}\"); let sh=tensor_shape(y); let v=tensor_values(y); assert(sh[0]==2 and sh[1]==2); assert(v[0]==1 and v[3]==4);"
        );
        assert!(run_source(&src).is_ok());
        let _ = fs::remove_file(path);
    }
    #[test]
    fn task_timeout_and_status() {
        let src = "fn slow()->Int{sleep_ms(25);return 7;} let t=spawn(slow,[]); let s=task_status(t); assert(s==\"running\" or s==\"finished\"); let early=await_timeout(t,1); assert(early==null); let value=await_task(t); assert(value==7);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn lsp_rename_helper() {
        let src = "fn add(x:Int)->Int{return x+1;} let a=add(1); let b=add(a);";
        let edit = lsp_rename(src, "file:///test.vy", "add", "increment");
        let changes = edit
            .get("changes")
            .and_then(|x| x.get("file:///test.vy"))
            .and_then(JsonValue::as_array)
            .map(|x| x.len())
            .unwrap_or(0);
        assert!(changes >= 3);
    }
    #[test]
    fn security_primitives() {
        let src = "let mac=hmac_sha256(\"key\",\"message\"); assert(len(mac)==64); let derived=pbkdf2_sha256(\"password\",\"salt\",100,32); assert(len(derived)==64); assert(constant_time_equal(mac,mac)); assert(not constant_time_equal(mac,derived));";
        assert!(run_source(src).is_ok());
    }
    #[cfg(feature = "native-services")]
    #[test]
    fn sqlite_parameter_binding() {
        let db = env::temp_dir().join("veyra_params_test.sqlite");
        let escaped = db.to_string_lossy().replace('\\', "\\\\");
        let src = format!(
            "sqlite_exec(\"{escaped}\",\"CREATE TABLE IF NOT EXISTS items(id INTEGER,name TEXT)\"); sqlite_exec(\"{escaped}\",\"DELETE FROM items\"); sqlite_exec_params(\"{escaped}\",\"INSERT INTO items(id,name) VALUES (?,?)\",[7,\"veyra\"]); let rows=sqlite_query_params(\"{escaped}\",\"SELECT id,name FROM items WHERE id=?\",[7]); assert(len(rows)==1); assert(rows[0][\"name\"]==\"veyra\");"
        );
        assert!(run_source(&src).is_ok());
        let _ = fs::remove_file(db);
    }
    #[test]
    fn static_server_rejects_path_traversal() {
        let root = env::temp_dir().join("veyra_static_traversal_test");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let index = root.join("index.html");
        fs::write(&index, "ok").unwrap();
        assert!(resolve_static_path(&root, "/").is_some());
        assert!(resolve_static_path(&root, "/index.html?x=1").is_some());
        assert!(resolve_static_path(&root, "/../secret.txt").is_none());
        assert!(resolve_static_path(&root, "/../../Windows/win.ini").is_none());
        let _ = fs::remove_dir_all(&root);
    }
    #[test]
    fn use_preserves_trailing_statements_on_same_line() {
        let dir = env::temp_dir().join("veyra_inline_use_test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mod_a = dir.join("a.vy");
        let mod_b = dir.join("b.vy");
        let main_vy = dir.join("main.vy");
        fs::write(&mod_a, "fn fa() -> Int { return 10; }").unwrap();
        fs::write(&mod_b, "fn fb() -> Int { return 20; }").unwrap();
        fs::write(
            &main_vy,
            "use \"a.vy\"; use \"b.vy\"; assert(fa() + fb() == 30);",
        )
        .unwrap();
        let loaded = load_source(main_vy.to_string_lossy().as_ref()).unwrap();
        assert!(loaded.contains("assert(fa() + fb() == 30);"));
        assert!(run_source(&loaded).is_ok());
        let _ = fs::remove_dir_all(&dir);
    }
    #[test]
    fn type_of_distinguishes_int_and_float() {
        let src = "assert(type_of(7)==\"Int\"); assert(type_of(2.5)==\"Number\");";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn short_circuit_boolean_operators() {
        let src = "
            fn boom() -> Bool {
                throw \"rhs should not evaluate\";
            }
            let a = false and boom();
            let b = true or boom();
            assert(a == false);
            assert(b == true);
        ";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn cross_runtime_syntax_parity() {
        let src = "
            /* block comment */
            # hash comment
            let msg = 'hello';
            assert(msg == \"hello\");

            let x = 10;
            x += 5;
            x -= 3;
            x *= 2;
            x /= 4;
            assert(x == 6);

            let label = '';
            if x == 1 {
                label = 'one';
            } else if x == 6 {
                label = 'six';
            } else {
                label = 'other';
            }
            assert(label == 'six');

            let xs = [10, 20, 30];
            xs[1] = 99;
            xs[2] += 1;
            assert(xs[1] == 99 and xs[2] == 31);

            let m = {'a': 1};
            m['b'] = 2;
            m['a'] += 10;
            assert(m['a'] == 11 and m['b'] == 2);

            let sum1 = 0;
            for i in range(5) {
                sum1 += i;
            }
            assert(sum1 == 10);

            let sum2 = 0;
            for i in range(1, 6, 2) {
                sum2 += i;
            }
            assert(sum2 == 9);
            assert(len(range(4)) == 4);
        ";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn power_tuples_and_set_literals() {
        let src = "
            assert(2 ** 3 ** 2 == 512);
            let t = (10, 20, 30);
            assert(len(t) == 3 and t[1] == 20);
            let s = {1, 2, 2, 3, 3};
            assert(len(s) == 3 and contains(s, 2));
        ";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn try_catch_unwinds_and_catches_runtime_errors() {
        let src = "
            let caught_oob = false;
            let xs = [1, 2, 3];
            try {
                let bad = 10 + xs[99];
            } catch err {
                caught_oob = contains(err, 'out of range');
            }
            assert(caught_oob);

            fn inner_fail() -> Int {
                throw 'nested-boom';
            }
            let caught_nested = '';
            try {
                let v = inner_fail();
            } catch err {
                caught_nested = err;
            }
            assert(caught_nested == 'nested-boom');
        ";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn line_aware_diagnostics_and_static_parent_check() {
        let bad_src = "let a = 1;\nlet b = 2;\nlet c = ;";
        let diags = lsp_diagnostics(bad_src);
        assert_eq!(diags.len(), 1);
        let line = diags[0]
            .pointer("/range/start/line")
            .and_then(JsonValue::as_u64)
            .unwrap_or(0);
        assert_eq!(line, 2);
        assert!(compile_source("class Dog: MissingParent {}").is_err());
    }
    #[test]
    fn bytecode_optimizer_folds_constants_and_preserves_semantics() {
        let src = "
            let x = 2 + 3 * 4 + 2 ** 3;
            let s = 'vey' + 'ra';
            let ok = (10 > 5) and (not false);
            assert(x == 22);
            assert(s == 'veyra');
            assert(ok == true);
        ";
        let unopt = compile_source_unoptimized(src).unwrap();
        let opt = compile_source(src).unwrap();
        assert!(opt.len() < unopt.len());
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn autograd_tape_reclaims_intermediate_tensors() {
        let src = "
            let x = tensor([[1], [2], [3], [4]], false);
            let y = tensor([[2], [4], [6], [8]], false);
            let w = tensor([[0]], true);
            for epoch in range(25) {
                let pred = tensor_matmul(x, w);
                let diff = tensor_sub(pred, y);
                let sq = tensor_mul(diff, diff);
                let loss = tensor_mean(sq);
                tensor_backward(loss);
                tensor_step(w, 0.05);
                tensor_zero_grad(w);
            }
            let reclaimed = tensor_gc();
            assert(reclaimed >= 0);
        ";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn mutable_closure_upvalues() {
        let src = "
            fn make_counter(start: Int) -> Function {
                let count = start;
                return fn() -> Int {
                    count += 1;
                    return count;
                };
            }
            let c1 = make_counter(0);
            let c2 = make_counter(10);
            assert(c1() == 1);
            assert(c1() == 2);
            assert(c2() == 11);
            assert(c1() == 3);

            let shared = 5;
            fn bump_shared(delta: Int) -> Int {
                shared += delta;
                return shared;
            }
            assert(bump_shared(7) == 12);
            assert(shared == 12);
        ";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn with_cleanup_on_exception() {
        let src = "
            class Guard {
                fn init(self) -> Null {
                    self.cleaned = false;
                    return null;
                }
                fn enter(self) -> Guard {
                    return self;
                }
                fn exit(self) -> Null {
                    self.cleaned = true;
                    return null;
                }
            }
            let g = Guard();
            let caught = false;
            try {
                with g as active {
                    assert(active.cleaned == false);
                    throw 'abort-inside-with';
                }
            } catch err {
                caught = (err == 'abort-inside-with');
            }
            assert(caught);
            assert(g.cleaned == true);
        ";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn vybc_round_trip_and_checksum_tamper_detection() {
        let src = "
            fn square(n: Int) -> Int { return n * n; }
            let total = 0;
            for i in range(1, 5) {
                total += square(i);
            }
            assert(total == 30);
        ";
        let vybc = compile_to_vybc(src).unwrap();
        assert!(run_vybc(&vybc).is_ok());
        let tampered = vybc.replace("\"Add\"", "\"Sub\"");
        assert!(run_vybc(&tampered).is_err());
    }
    #[test]
    fn self_hosted_compiler_three_stage_fixpoint() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let compiler_path = root.join("compiler-vy").join("src").join("main.vy");
        let compiler_src = fs::read_to_string(&compiler_path).unwrap();
        assert!(!compiler_src.contains("vybc_compile"));

        // Stage 0 (Rust Horizon engine) compiles compiler-vy into stage1.vybc
        let stage1_vybc = compile_to_vybc(&compiler_src).unwrap();
        assert!(run_vybc(&stage1_vybc).is_ok());

        // Stage 1 executes compiler-vy to compile a full Veyra program (functions, loops, branches, lists, maps)
        // into .vybc AND compiles compiler-vy itself (Stage 1 -> Stage 2) and then runs Stage 2 to compile
        // compiler-vy again (Stage 2 -> Stage 3), verifying Stage 2 == Stage 3 fixpoint equality.
        let compiler_path_str = compiler_path.to_string_lossy().replace('\\', "/");
        let stage3_tmp = std::env::temp_dir()
            .join("veyra_stage3_fixpoint.vybc")
            .to_string_lossy()
            .replace('\\', "/");
        let driver = format!(
            "{compiler_src}\n\
            let prog = \"fn fib(n: Int) -> Int {{ if n <= 1 {{ return n; }} return fib(n - 1) + fib(n - 2); }} let acc = 0; for x in [1, 2, 3, 4, 5, 6] {{ acc += fib(x); }} let m = {{\\\"sum\\\": acc}}; assert(m[\\\"sum\\\"] == 20);\";\n\
            let target_bc = compile_veyra_to_vybc(prog);\n\
            vybc_run(target_bc);\n\
            let self_src = read_file(\"{compiler_path_str}\");\n\
            let stage2_bc = bootstrap_compile_self(self_src);\n\
            let stage2_driver = self_src + \"\\nlet s = read_file(\\\"{compiler_path_str}\\\");\\nlet stage3_bc = bootstrap_compile_self(s);\\nwrite_file(\\\"{stage3_tmp}\\\", stage3_bc);\\n\";\n\
            let stage2_runner_bc = compile_veyra_to_vybc(stage2_driver);\n\
            vybc_run(stage2_runner_bc);\n\
            let stage3_bc = read_file(\"{stage3_tmp}\");\n\
            assert(stage2_bc == stage3_bc);\n\
            assert(sha256(stage2_bc) == sha256(stage3_bc));\n"
        );
        run_source(&driver).unwrap();
        let _ = fs::remove_file(&stage3_tmp);
    }
    #[test]
    fn tensor_broadcasting_and_backward_reduction() {
        let src = "
            let x = tensor([[1, 2], [3, 4], [5, 6]], true);
            let b = tensor([10, 20], true);
            let s = tensor([[2, 3]], true);
            let y = tensor_add(tensor_mul(x, s), b);
            assert(tensor_shape(y) == [3, 2]);
            assert(tensor_values(y) == [12, 26, 16, 32, 20, 38]);
            let loss = tensor_sum(y);
            tensor_backward(loss);
            assert(tensor_grad(b) == [3, 3]);
            assert(tensor_grad(s) == [9, 12]);
            assert(tensor_grad(x) == [2, 3, 2, 3, 2, 3]);
        ";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn python_parity_fstrings_slicing_methods_and_destructuring() {
        let src = r#"
            # 1. F-strings & expressions inside interpolation
            let name = "veyra"
            let score = 95
            let greeting = f"Hello {name.upper()}, score={score + 5}!"
            assert(greeting == "Hello VEYRA, score=100!")

            # 2. Negative indexing & Python slicing [start:stop:step]
            let nums = [10, 20, 30, 40, 50]
            assert(nums[-1] == 50)
            assert(nums[-2] == 40)
            nums[-1] = 55
            assert(nums[4] == 55)
            assert(nums[1:4] == [20, 30, 40])
            assert(nums[:2] == [10, 20])
            assert(nums[3:] == [40, 55])
            assert(nums[::-1] == [55, 40, 30, 20, 10])
            assert("abcde"[::-1] == "edcba")
            assert("abcde"[-1] == "e")

            # 3. Python 'in' / 'not in', 'def', 'elif', 'None', 'True', 'False', 'lambda'
            def classify(x) {
                if x in [1, 2, 3] {
                    return "small"
                } elif x not in [10, 20] and x is not None {
                    return "medium"
                } else {
                    pass
                    return "other"
                }
            }
            assert(classify(2) == "small")
            assert(classify(5) == "medium")
            let sq = lambda z: z * z
            assert(sq(9) == 81)

            # 4. List & Map Python methods + Destructuring + For-loop unpacking
            let items = [3, 1, 2]
            items.append(4)
            items.sort()
            assert(items == [1, 2, 3, 4])
            items.reverse()
            assert(items == [4, 3, 2, 1])

            let a, b = [100, 200]
            assert(a == 100 and b == 200)
            let [c, d] = ["x", "y"]
            assert(c == "x" and d == "y")

            let dmap = {"alpha": 1, "beta": 2}
            assert(dmap.get("alpha") == 1)
            assert(dmap.get("missing", 99) == 99)
            let sum_v = 0
            for k, v in dmap.items() {
                if "a" in k {
                    sum_v += v
                }
            }
            assert(sum_v == 3)
            assert(chr(65) == "A" and ord("A") == 65)
            assert(hex(255) == "0xff" and bin(5) == "0b101")
        "#;
        run_source(src).unwrap();
    }
    #[test]
    fn python_parity_defaults_varargs_ternary_and_comprehensions() {
        let src = r#"
            def greet(name, greeting = "Hello", punct = "!") {
                return greeting + ", " + name + punct
            }
            assert(greet("Ana") == "Hello, Ana!")
            assert(greet("Bo", "Hi") == "Hi, Bo!")
            assert(greet("Cy", "Yo", "?") == "Yo, Cy?")

            def total(first, *rest) {
                let s = first
                for r in rest { s += r }
                return s
            }
            assert(total(1) == 1)
            assert(total(1, 2, 3, 4) == 10)

            let n = 7
            let parity = "even" if n % 2 == 0 else "odd"
            assert(parity == "odd")
            let sign = "neg" if n < 0 else "zero" if n == 0 else "pos"
            assert(sign == "pos")

            let squares = {x: x * x for x in [1, 2, 3]}
            assert(squares[3] == 9 and len(squares) == 3)
            let evens = {x % 3 for x in [1, 2, 3, 4, 5, 6]}
            assert(len(evens) == 3)
            let picked = [x for x in range(10) if x % 4 == 0]
            assert(picked == [0, 4, 8])

            assert([1, 2] + [3] == [1, 2, 3])
            assert("ab" * 3 == "ababab")
            assert([0] * 3 == [0, 0, 0])
            assert(list("abc") == ["a", "b", "c"])
            assert(dict([["k", 1]])["k"] == 1)

            class Counter {
                fn init(self) -> Null { self.n = 0; return null; }
                fn tick(self) -> Int { self.n += 1; return self.n; }
            }
            let c = Counter()
            let i = 0
            while i < 50 { c.tick(); i += 1 }
            assert(c.n == 50)
        "#;
        run_source(src).unwrap();
    }
    #[test]
    fn closures_capture_only_referenced_names() {
        let src = "
            let big = [1, 2, 3];
            let unrelated = 5;
            let counter = 0;
            fn bump() -> Int { counter += 1; return counter; }
            bump(); bump();
            assert(counter == 2);
            unrelated += 1;
            let acc = 0;
            for k in range(1000) { acc += k; }
            assert(acc == 499500 and unrelated == 6 and len(big) == 3);
        ";
        run_source(src).unwrap();
    }
    #[test]
    fn standalone_bundle_round_trip() {
        let payload = compile_to_vybc("let x = 40 + 2; assert(x == 42);").unwrap();
        let mut fake_exe = b"MZ_FAKE_PE_HEADER".to_vec();
        fake_exe.extend_from_slice(payload.as_bytes());
        fake_exe.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        fake_exe.extend_from_slice(BUNDLE_MAGIC);
        let extracted = extract_embedded_vybc(&fake_exe).expect("should extract embedded vybc");
        assert_eq!(extracted, payload);
        assert!(run_vybc(&extracted).is_ok());
    }
    #[test]
    fn ml_layers_forward_and_autograd() {
        let src = r#"
            // 1D Convolution forward & backward
            let x1 = tensor([[[1.0, 2.0, 3.0, 4.0]]], true);
            let w1 = tensor([[[1.0, 0.0, -1.0]]], true);
            let y1 = tensor_conv1d(x1, w1);
            assert(tensor_shape(y1) == [1, 1, 2]);
            let v1 = tensor_values(y1);
            assert(v1[0] == -2.0 and v1[1] == -2.0);
            let loss1 = tensor_sum(tensor_mul(y1, tensor([[[2.0, 3.0]]], false)));
            tensor_backward(loss1);
            // dy1 = [2, 3] => dx1[0]=2, dx1[1]=3, dx1[2]=-2, dx1[3]=-3; dw1=[1*2+2*3=8, 2*2+3*3=13, 3*2+4*3=18]
            assert(tensor_grad(x1) == [2.0, 3.0, -2.0, -3.0]);
            assert(tensor_grad(w1) == [8.0, 13.0, 18.0]);

            // 2D Convolution forward & backward
            let x2 = tensor([[[[1.0, 2.0, 3.0], [4.0, 5.0, 6.0]]]], true);
            let w2 = tensor([[[[1.0, 0.0], [0.0, 1.0]]]], true);
            let y2 = tensor_conv2d(x2, w2);
            assert(tensor_shape(y2) == [1, 1, 1, 2]);
            assert(tensor_values(y2) == [6.0, 8.0]);
            tensor_backward(tensor_sum(y2));
            assert(tensor_grad(w2) == [3.0, 5.0, 9.0, 11.0]);
            assert(tensor_grad(x2) == [1.0, 1.0, 0.0, 0.0, 1.0, 1.0]);

            // LayerNorm forward + finite-difference gradient check
            let xln = tensor([[1.0, 3.0, 2.0], [4.0, 0.0, -1.0]], true);
            let wln = tensor([[1.0, 2.0, 3.0], [0.5, -1.5, 2.0]], false);
            let yln = tensor_layer_norm(xln, 0.000001);
            assert(tensor_shape(yln) == [2, 3]);
            let lln = tensor_sum(tensor_mul(yln, wln));
            tensor_backward(lln);
            let gln = tensor_grad(xln);
            let eps = 0.00001;
            let num_ln = (
                tensor_values(tensor_sum(tensor_mul(tensor_layer_norm(tensor([[1.0 + eps, 3.0, 2.0], [4.0, 0.0, -1.0]], false), 0.000001), wln)))[0]
                - tensor_values(tensor_sum(tensor_mul(tensor_layer_norm(tensor([[1.0 - eps, 3.0, 2.0], [4.0, 0.0, -1.0]], false), 0.000001), wln)))[0]
            ) / (2.0 * eps);
            assert(abs(gln[0] - num_ln) < 0.0001);

            // Cross-Entropy loss + gradient check
            let logits = tensor([[2.0, 0.5, -1.0], [0.1, 1.5, 0.2]], true);
            let ce = tensor_cross_entropy(logits, [0, 1]);
            assert(tensor_shape(ce) == []);
            assert(tensor_values(ce)[0] > 0.0 and tensor_values(ce)[0] < 1.0);
            tensor_backward(ce);
            let gce = tensor_grad(logits);
            let num_ce = (
                tensor_values(tensor_cross_entropy(tensor([[2.0 + eps, 0.5, -1.0], [0.1, 1.5, 0.2]], false), [0, 1]))[0]
                - tensor_values(tensor_cross_entropy(tensor([[2.0 - eps, 0.5, -1.0], [0.1, 1.5, 0.2]], false), [0, 1]))[0]
            ) / (2.0 * eps);
            assert(abs(gce[0] - num_ce) < 0.0001);

            // Multi-head Attention forward & finite-difference gradient check
            let q = tensor([[1.0, 0.5, -0.3, 0.8], [0.2, -0.4, 0.6, 0.1]], true);
            let k = tensor([[0.4, 1.0, 0.2, -0.5], [-0.2, 0.3, 0.7, 0.9]], true);
            let v = tensor([[1.0, 2.0, 3.0, 4.0], [0.5, -1.0, 1.5, 2.5]], true);
            let wt = tensor([[1.0, -0.5, 0.25, 0.75], [-0.2, 0.8, 0.4, -0.6]], false);
            let mha = tensor_multihead_attention(q, k, v, 2);
            assert(tensor_shape(mha) == [2, 4]);
            let lmha = tensor_sum(tensor_mul(mha, wt));
            tensor_backward(lmha);
            let gq = tensor_grad(q);
            let gk = tensor_grad(k);
            let gv = tensor_grad(v);
            let qp = tensor([[1.0 + eps, 0.5, -0.3, 0.8], [0.2, -0.4, 0.6, 0.1]], false);
            let qm = tensor([[1.0 - eps, 0.5, -0.3, 0.8], [0.2, -0.4, 0.6, 0.1]], false);
            let num_q = (
                tensor_values(tensor_sum(tensor_mul(tensor_multihead_attention(qp, k, v, 2), wt)))[0]
                - tensor_values(tensor_sum(tensor_mul(tensor_multihead_attention(qm, k, v, 2), wt)))[0]
            ) / (2.0 * eps);
            assert(abs(gq[0] - num_q) < 0.0001);
            let kp = tensor([[0.4 + eps, 1.0, 0.2, -0.5], [-0.2, 0.3, 0.7, 0.9]], false);
            let km = tensor([[0.4 - eps, 1.0, 0.2, -0.5], [-0.2, 0.3, 0.7, 0.9]], false);
            let num_k = (
                tensor_values(tensor_sum(tensor_mul(tensor_multihead_attention(q, kp, v, 2), wt)))[0]
                - tensor_values(tensor_sum(tensor_mul(tensor_multihead_attention(q, km, v, 2), wt)))[0]
            ) / (2.0 * eps);
            assert(abs(gk[0] - num_k) < 0.0001);
            let vp = tensor([[1.0 + eps, 2.0, 3.0, 4.0], [0.5, -1.0, 1.5, 2.5]], false);
            let vm = tensor([[1.0 - eps, 2.0, 3.0, 4.0], [0.5, -1.0, 1.5, 2.5]], false);
            let num_v = (
                tensor_values(tensor_sum(tensor_mul(tensor_multihead_attention(q, k, vp, 2), wt)))[0]
                - tensor_values(tensor_sum(tensor_mul(tensor_multihead_attention(q, k, vm, 2), wt)))[0]
            ) / (2.0 * eps);
            assert(abs(gv[0] - num_v) < 0.0001);
        "#;
        run_source(src).unwrap();
    }
    #[test]
    fn ml_checkpoint_save_and_load() {
        let path = env::temp_dir().join("veyra_ml_checkpoint_test.json");
        let escaped = path.to_string_lossy().replace('\\', "\\\\");
        let src = format!(
            r#"
            let w = tensor([[1.5, -2.0], [0.25, 3.0]], true);
            let b = tensor([0.5, -0.5], true);
            let n = tensor_checkpoint_save({{"w": w, "b": b}}, "{escaped}");
            assert(n == 2);
            let ckpt = tensor_checkpoint_load("{escaped}");
            assert(tensor_shape(ckpt["w"]) == [2, 2]);
            assert(tensor_values(ckpt["w"]) == [1.5, -2.0, 0.25, 3.0]);
            assert(tensor_values(ckpt["b"]) == [0.5, -0.5]);
            tensor_backward(tensor_sum(ckpt["w"]));
            assert(tensor_grad(ckpt["w"]) == [1.0, 1.0, 1.0, 1.0]);
            "#
        );
        run_source(&src).unwrap();
        let _ = fs::remove_file(path);
    }
    #[test]
    fn lsp_async_fn_definition_and_completion_include_ml() {
        let src =
            "async fn fetch_data(x: Int) -> Int {\n    return x + 1;\n}\nlet y = fetch_data(2);";
        let def = lsp_definition(src, "file:///test.vy", "fetch_data");
        assert_eq!(
            def.pointer("/range/start/line").and_then(|v| v.as_u64()),
            Some(0)
        );
        let comps = crate::lsp::lsp_completion_items();
        let labels: Vec<&str> = comps
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|x| x.get("label").and_then(|v| v.as_str()))
            .collect();
        assert!(labels.contains(&"tensor_conv2d"));
        assert!(labels.contains(&"tensor_multihead_attention"));
        assert!(labels.contains(&"tensor_checkpoint_save"));
    }
    #[test]
    fn dap_protocol_breakpoints_stepping_and_variables() {
        use std::collections::VecDeque;
        use std::io::{BufReader, Read, Write};
        use std::sync::{Arc, Condvar, Mutex};

        #[derive(Clone, Default)]
        struct MemPipe {
            inner: Arc<(Mutex<(VecDeque<u8>, bool)>, Condvar)>,
        }
        impl MemPipe {
            fn close(&self) {
                let (m, cv) = &*self.inner;
                let mut g = m.lock().unwrap();
                g.1 = true;
                cv.notify_all();
            }
        }
        impl Read for MemPipe {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let (m, cv) = &*self.inner;
                let mut g = m.lock().unwrap();
                while g.0.is_empty() && !g.1 {
                    g = cv.wait(g).unwrap();
                }
                let mut n = 0;
                while n < buf.len() {
                    if let Some(b) = g.0.pop_front() {
                        buf[n] = b;
                        n += 1;
                    } else {
                        break;
                    }
                }
                Ok(n)
            }
        }
        impl Write for MemPipe {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                let (m, cv) = &*self.inner;
                let mut g = m.lock().unwrap();
                g.0.extend(buf);
                cv.notify_all();
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let prog = env::temp_dir().join("veyra_dap_test.vy");
        fs::write(
            &prog,
            "fn add_step(x, y) {\n    let z = x + y;\n    return z * 2;\n}\nlet a = 3;\nlet b = 4;\nlet res = add_step(a, b);\nprint(\"res\", res);\nassert(res == 14);\n",
        )
        .unwrap();

        let mut c2s = MemPipe::default();
        let s2c = MemPipe::default();
        let server_in = c2s.clone();
        let server_out = s2c.clone();
        let srv_handle = std::thread::spawn(move || {
            crate::dap::run_dap_with_io(BufReader::new(server_in), Box::new(server_out))
        });

        let mut client_reader = BufReader::new(s2c);
        let mut seq = 1u64;
        let mut send_req = |cmd: &str, args: serde_json::Value| -> u64 {
            let id = seq;
            seq += 1;
            let payload = serde_json::json!({
                "seq": id,
                "type": "request",
                "command": cmd,
                "arguments": args
            });
            let body = serde_json::to_string(&payload).unwrap();
            let msg = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
            c2s.write_all(msg.as_bytes()).unwrap();
            c2s.flush().unwrap();
            id
        };
        let mut events: Vec<serde_json::Value> = Vec::new();
        fn recv_resp(
            reader: &mut impl std::io::BufRead,
            events: &mut Vec<serde_json::Value>,
            target_cmd: &str,
        ) -> serde_json::Value {
            loop {
                let msg = crate::dap::read_dap_message(reader)
                    .unwrap()
                    .expect("expected DAP message");
                if msg.get("type").and_then(|v| v.as_str()) == Some("response")
                    && msg.get("command").and_then(|v| v.as_str()) == Some(target_cmd)
                {
                    assert_eq!(
                        msg.get("success").and_then(|v| v.as_bool()),
                        Some(true),
                        "DAP command {target_cmd} failed: {msg}"
                    );
                    return msg;
                }
                events.push(msg);
            }
        }
        fn wait_event(
            reader: &mut impl std::io::BufRead,
            events: &mut Vec<serde_json::Value>,
            target_event: &str,
        ) -> serde_json::Value {
            if let Some(pos) = events
                .iter()
                .position(|m| m.get("event").and_then(|v| v.as_str()) == Some(target_event))
            {
                return events.remove(pos);
            }
            loop {
                let msg = crate::dap::read_dap_message(reader)
                    .unwrap()
                    .expect("expected DAP event");
                if msg.get("type").and_then(|v| v.as_str()) == Some("event")
                    && msg.get("event").and_then(|v| v.as_str()) == Some(target_event)
                {
                    return msg;
                }
                events.push(msg);
            }
        }

        send_req("initialize", serde_json::json!({"adapterID": "veyra"}));
        let _ = recv_resp(&mut client_reader, &mut events, "initialize");
        let _ = wait_event(&mut client_reader, &mut events, "initialized");

        send_req(
            "launch",
            serde_json::json!({"program": prog.to_string_lossy(), "stopOnEntry": false}),
        );
        let _ = recv_resp(&mut client_reader, &mut events, "launch");

        send_req(
            "setBreakpoints",
            serde_json::json!({
                "source": {"path": prog.to_string_lossy()},
                "breakpoints": [{"line": 2}, {"line": 8}]
            }),
        );
        let bp_resp = recv_resp(&mut client_reader, &mut events, "setBreakpoints");
        let bps = bp_resp
            .pointer("/body/breakpoints")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(bps.len(), 2);
        assert_eq!(bps[0].get("verified").and_then(|v| v.as_bool()), Some(true));

        send_req("configurationDone", serde_json::json!({}));
        let _ = recv_resp(&mut client_reader, &mut events, "configurationDone");

        // Hit breakpoint at line 2 inside add_step(3, 4)
        let stop1 = wait_event(&mut client_reader, &mut events, "stopped");
        assert_eq!(
            stop1.pointer("/body/reason").and_then(|v| v.as_str()),
            Some("breakpoint")
        );

        send_req("stackTrace", serde_json::json!({"threadId": 1}));
        let st1 = recv_resp(&mut client_reader, &mut events, "stackTrace");
        let frames1 = st1
            .pointer("/body/stackFrames")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(frames1.len(), 2);
        assert_eq!(
            frames1[0].get("name").and_then(|v| v.as_str()),
            Some("add_step")
        );
        assert_eq!(frames1[0].get("line").and_then(|v| v.as_u64()), Some(2));
        assert_eq!(
            frames1[1].get("name").and_then(|v| v.as_str()),
            Some("<main>")
        );
        let top_fid = frames1[0].get("id").and_then(|v| v.as_u64()).unwrap();

        send_req("scopes", serde_json::json!({"frameId": top_fid}));
        let sc1 = recv_resp(&mut client_reader, &mut events, "scopes");
        let scopes1 = sc1.pointer("/body/scopes").unwrap().as_array().unwrap();
        assert!(scopes1.len() >= 2);
        let locals_ref = scopes1[0]
            .get("variablesReference")
            .unwrap()
            .as_u64()
            .unwrap();

        send_req(
            "variables",
            serde_json::json!({"variablesReference": locals_ref}),
        );
        let vars1 = recv_resp(&mut client_reader, &mut events, "variables");
        let vlist1 = vars1
            .pointer("/body/variables")
            .unwrap()
            .as_array()
            .unwrap();
        assert!(vlist1.iter().any(|v| v["name"] == "x" && v["value"] == "3"));
        assert!(vlist1.iter().any(|v| v["name"] == "y" && v["value"] == "4"));

        send_req(
            "evaluate",
            serde_json::json!({"expression": "x * 10 + y", "frameId": top_fid}),
        );
        let eval_resp = recv_resp(&mut client_reader, &mut events, "evaluate");
        assert_eq!(
            eval_resp.pointer("/body/result").and_then(|v| v.as_str()),
            Some("34")
        );

        // Step over to line 3 inside add_step; z should now be 7
        send_req("next", serde_json::json!({"threadId": 1}));
        let _ = recv_resp(&mut client_reader, &mut events, "next");
        let stop2 = wait_event(&mut client_reader, &mut events, "stopped");
        assert_eq!(
            stop2.pointer("/body/reason").and_then(|v| v.as_str()),
            Some("step")
        );

        send_req("stackTrace", serde_json::json!({"threadId": 1}));
        let st2 = recv_resp(&mut client_reader, &mut events, "stackTrace");
        let frames2 = st2
            .pointer("/body/stackFrames")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(frames2[0].get("line").and_then(|v| v.as_u64()), Some(3));
        let top_fid2 = frames2[0].get("id").and_then(|v| v.as_u64()).unwrap();

        send_req("scopes", serde_json::json!({"frameId": top_fid2}));
        let sc2 = recv_resp(&mut client_reader, &mut events, "scopes");
        let locals_ref2 = sc2
            .pointer("/body/scopes/0/variablesReference")
            .unwrap()
            .as_u64()
            .unwrap();
        send_req(
            "variables",
            serde_json::json!({"variablesReference": locals_ref2}),
        );
        let vars2 = recv_resp(&mut client_reader, &mut events, "variables");
        assert!(
            vars2
                .pointer("/body/variables")
                .unwrap()
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v["name"] == "z" && v["value"] == "7")
        );

        // Continue to line 8 breakpoint in <main>
        send_req("continue", serde_json::json!({"threadId": 1}));
        let _ = recv_resp(&mut client_reader, &mut events, "continue");
        let stop3 = wait_event(&mut client_reader, &mut events, "stopped");
        assert_eq!(
            stop3.pointer("/body/reason").and_then(|v| v.as_str()),
            Some("breakpoint")
        );

        send_req("stackTrace", serde_json::json!({"threadId": 1}));
        let st3 = recv_resp(&mut client_reader, &mut events, "stackTrace");
        let frames3 = st3
            .pointer("/body/stackFrames")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(frames3.len(), 1);
        assert_eq!(frames3[0].get("line").and_then(|v| v.as_u64()), Some(8));

        send_req("continue", serde_json::json!({"threadId": 1}));
        let _ = recv_resp(&mut client_reader, &mut events, "continue");
        let out_ev = wait_event(&mut client_reader, &mut events, "output");
        assert!(
            out_ev
                .pointer("/body/output")
                .and_then(|v| v.as_str())
                .unwrap()
                .contains("res 14")
        );
        let exited = wait_event(&mut client_reader, &mut events, "exited");
        assert_eq!(
            exited.pointer("/body/exitCode").and_then(|v| v.as_u64()),
            Some(0)
        );
        let _ = wait_event(&mut client_reader, &mut events, "terminated");

        send_req("disconnect", serde_json::json!({}));
        let _ = recv_resp(&mut client_reader, &mut events, "disconnect");
        c2s.close();
        srv_handle.join().unwrap().unwrap();
        let _ = fs::remove_file(prog);
    }

    #[test]
    fn phase3b_flat_frames_indexed_locals_and_vybc_roundtrip() {
        let src = r#"
fn recurse(n: Int) -> Int {
    if n <= 0 {
        return 0;
    }
    return 1 + recurse(n - 1);
}
assert(recurse(5000) == 5000);

fn inner_fail(x: Int) -> Int {
    let local_y = x + 1;
    if local_y > 5 {
        throw "unwind-ok";
    }
    return local_y;
}
fn outer_call(a: Int) -> Int {
    let base = 10;
    let r = inner_fail(a);
    return base + r;
}
let caught = "";
try {
    outer_call(10);
} catch e {
    caught = e;
}
assert(caught == "unwind-ok");

fn apply_fn(f, v: Int) -> Int {
    return f(v);
}
fn make_acc(start: Int) {
    let local_only = 10;
    let shared = start;
    fn step(delta: Int) -> Int {
        shared = shared + delta;
        return shared;
    }
    let a = apply_fn(step, 5);
    let b = step(3);
    return local_only + a + b;
}
assert(make_acc(2) == 10 + 7 + 10);

fn sum_loop(n: Int) -> Int {
    let total = 0;
    let i = 0;
    while i < n {
        total = total + i;
        i = i + 1;
    }
    return total;
}
assert(sum_loop(100) == 4950);
"#;
        assert_eq!(run_source(src), Ok(()));

        let vybc = compile_to_vybc(src).unwrap();
        let decoded = crate::bytecode::decode_bytecode(&vybc).unwrap();
        let mut saw_local_op = false;
        for op in &decoded {
            if let Op::Def(_, _, _, _, body) = op {
                if body
                    .iter()
                    .any(|inner| matches!(inner, Op::LoadLocal(_) | Op::DefineLocal(_, _, _)))
                {
                    saw_local_op = true;
                }
            }
        }
        assert!(saw_local_op);
        assert_eq!(run_vybc(&vybc), Ok(()));
    }

    #[test]
    fn aot_compiles_and_matches_vm_semantics() {
        let src = r#"
const SCALE = 2;

fn fib(n: Int) -> Int {
    if n <= 1 {
        return n;
    }
    return fib(n - 1) + fib(n - 2);
}

fn calc_def(base: Int, bonus = 5) -> Int {
    return (base + bonus) * SCALE;
}

fn calc_var(base: Int, *rest) -> Int {
    let total = base * SCALE;
    for r in rest {
        total = total + r;
    }
    return total;
}

fn make_counter(start: Int) {
    let count = start;
    fn bump(delta: Int) -> Int {
        count = count + delta;
        return count;
    }
    fn inner_shadow() -> Int {
        let count = 99;
        return count;
    }
    return [bump(2), inner_shadow(), bump(3)];
}

class Animal {
    fn init(self, name: String) {
        self.name = name;
    }
    fn label(self) -> String {
        return f"animal:{self.name}";
    }
}

class Dog: Animal {
    fn speak(self) -> String {
        return self.label() + "!";
    }
}

class Ctx {
    fn init(self) {
        self.closed = false;
    }
    fn enter(self) {
        return self;
    }
    fn exit(self) {
        self.closed = true;
    }
}

fn add_tag(f) {
    return fn(x) { return f(x) + 100; };
}

@add_tag
fn triple(x: Int) -> Int {
    return x * 3;
}

fn fail_inner() {
    throw "boom";
}

let c_res = make_counter(10);
let d = Dog("Rex");
let xs = [1, 2, 3];
xs[1] = 20;
let evens = [x * SCALE for x in [1, 2, 3, 4] if x % 2 == 0];

let caught = "";
let fin = false;
try {
    fail_inner();
} catch e {
    caught = e;
} finally {
    fin = true;
}

let m_out = "";
match 2 {
    case 1 { m_out = "one"; }
    case 2 { m_out = "two"; }
    case _ { m_out = "other"; }
}

let ctx = Ctx();
with ctx as c {
    assert(c.closed == false);
}

let loop_sum = 0;
for i in range(0, 6) {
    if i == 2 { continue; }
    if i == 5 { break; }
    loop_sum = loop_sum + i;
}
let each_out = "";
for ch in ["a", "b", "c"] {
    each_out = each_out + ch;
}

let parity = "big" if fib(10) > 50 else "small";
let res = [
    fib(10),
    calc_def(3),
    calc_var(3, 1, 4, 5),
    c_res,
    d.speak(),
    is_instance(d, "Animal"),
    xs,
    xs[-1],
    evens,
    triple(4),
    caught,
    fin,
    m_out,
    ctx.closed,
    loop_sum,
    each_out,
    parity
];
print("AOT_OUT", res);
"#;

        assert_eq!(run_source(src), Ok(()));

        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = env::temp_dir().join(format!("veyra_aot_test_{stamp}"));
        fs::create_dir_all(&dir).unwrap();
        let src_path = dir.join("prog.vy");
        let exe_path = dir.join(if cfg!(target_os = "windows") {
            "prog.exe"
        } else {
            "prog"
        });
        fs::write(&src_path, src).unwrap();

        let built = crate::aot::compile_file_to_native(&src_path, Some(&exe_path)).unwrap();
        assert!(built.exists());

        let native_out = std::process::Command::new(&built).output().unwrap();
        assert!(
            native_out.status.success(),
            "native stderr: {}",
            String::from_utf8_lossy(&native_out.stderr)
        );
        let stdout = String::from_utf8_lossy(&native_out.stdout);
        let expected = "AOT_OUT [55, 16, 16, [12, 99, 15], animal:Rex!, true, [1, 20, 3], 3, [4, 8], 112, boom, true, two, true, 8, abc, big]";
        assert!(
            stdout.lines().any(|l| l.trim() == expected),
            "unexpected native stdout: {stdout}"
        );

        let async_rust = crate::aot::compile_source_to_rust("async fn f() { return 1; }").unwrap();
        assert!(async_rust.contains("store_task"));

        let proj_dir = dir.join("proj");
        fs::create_dir_all(proj_dir.join("src")).unwrap();
        fs::write(
            proj_dir.join("veyra.json"),
            r#"{"name":"sample","version":"0.1.0","entry":"src/main.vy"}"#,
        )
        .unwrap();
        fs::write(
            proj_dir.join("src/main.vy"),
            "fn sq(x: Int) -> Int { return x * x; }\nprint(\"PROJ\", sq(9));\n",
        )
        .unwrap();
        crate::project::build_project_at(&proj_dir, true).unwrap();
        let proj_bin =
            proj_dir
                .join("dist")
                .join("sample-0.1.0")
                .join(if cfg!(target_os = "windows") {
                    "sample.exe"
                } else {
                    "sample"
                });
        assert!(proj_bin.exists());
        let proj_out = std::process::Command::new(&proj_bin).output().unwrap();
        assert!(proj_out.status.success());
        assert_eq!(String::from_utf8_lossy(&proj_out.stdout).trim(), "PROJ 81");

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn python_parity_calls_kwargs_generators_and_dunders() {
        let src = r#"
fn make_adder(n) {
    return lambda x: x + n;
}
let add5 = make_adder(5);
let funcs = [make_adder(10), lambda z: z * 2];
assert(make_adder(5)(3) == 8);
assert(add5(7) == 12);
assert(funcs[0](4) == 14);
assert((lambda x: x * 3)(7) == 21);

let evals = 0;
fn bump(v) {
    evals = evals + 1;
    return v;
}
assert(0 <= bump(5) < 10);
assert(evals == 1);
assert(!(10 < bump(20) < 15));
assert(evals == 2);

fn cfg(a, b=10, *rest, **kw) {
    return [a, b, len(rest), kw["tag"], kw["extra"]];
}
let res = cfg(1, 20, 30, 40, tag="v1", extra=99);
assert(res == [1, 20, 2, "v1", 99]);
let extra_kw = {"tag": "spread", "extra": 77};
assert(cfg(2, *[8, 9], **extra_kw) == [2, 8, 1, "spread", 77]);

let ev_sum = sum(x * x for x in range(6) if x % 2 == 0);
assert(ev_sum == 20);
let lazy = (x + 10 for x in range(4));
assert(next(lazy) == 10);
assert(collect(lazy) == [11, 12, 13]);

class Vec2 {
    fn init(self, x, y) {
        self.x = x;
        self.y = y;
    }
    fn __add__(self, o) {
        return Vec2(self.x + o.x, self.y + o.y);
    }
    fn __sub__(self, o) {
        return Vec2(self.x - o.x, self.y - o.y);
    }
    fn __mul__(self, k) {
        return Vec2(self.x * k, self.y * k);
    }
    fn __eq__(self, o) {
        return self.x == o.x and self.y == o.y;
    }
    fn __str__(self) {
        return f"V({self.x},{self.y})";
    }
    fn __repr__(self) {
        return f"Vec2({self.x}, {self.y})";
    }
    fn __len__(self) {
        return 2;
    }
    fn __call__(self, dx, dy=0) {
        return Vec2(self.x + dx, self.y + dy);
    }
    fn __getitem__(self, i) {
        return self.x if i == 0 else self.y;
    }
    fn __setitem__(self, i, val) {
        if i == 0 { self.x = val; } else { self.y = val; }
    }
}

let a = Vec2(1, 2);
let b = Vec2(3, 4);
let c = a + b;
assert(c == Vec2(4, 6));
assert((b - a) == Vec2(2, 2));
assert((a * 3) == Vec2(3, 6));
assert(str(c) == "V(4,6)");
assert(repr(c) == "Vec2(4, 6)");
assert(len(c) == 2);
assert(c(5, dy=1) == Vec2(9, 7));
assert(c[0] == 4 and c[1] == 6);
c[0] = 42;
assert(c[0] == 42);
assert(callable(c) and callable(make_adder) and !callable(42));
assert(hasattr(a, "x") and getattr(a, "x") == 1);
setattr(a, "z", 99);
assert(getattr(a, "z") == 99 and getattr(a, "missing", "fallback") == "fallback");

assert(divmod(17, 5) == [3, 2]);
assert(divmod(-7, 3) == [-3, 2]);
assert(round(3.14159, 2) == 3.14);
assert(pow(2, 10, 100) == 24);
let words = ["pear", "a", "kiwi", "banana"];
assert(sorted(words, key=lambda s: len(s), reverse=true) == ["banana", "pear", "kiwi", "a"]);
assert(min(words, key=lambda s: len(s)) == "a");
assert(max(words, key=lambda s: len(s)) == "banana");
assert(enumerate(["a", "b"], start=1) == [[1, "a"], [2, "b"]]);
assert(zip([1, 2], ["x", "y"], [true, false]) == [[1, "x", true], [2, "y", false]]);
assert("-".join(str(n) for n in range(3)) == "0-1-2");
assert("hello world".title() == "Hello World");
assert("42".zfill(5) == "00042");
assert("banana".rfind("a") == 5);
assert("abc123".isalnum() and "   ".isspace());

let m = {"a": 1, "b": 2};
m.update({"b": 20, "c": 30});
assert(m.pop("a") == 1);
assert(m.setdefault("d", 40) == 40);
assert(m.copy()["c"] == 30);
"#;
        assert_eq!(run_source(src), Ok(()));
    }

    #[test]
    fn python_bridge_vm_interop() {
        let src = r#"
assert(py_available());
let m = py_import("math");
assert(m.sqrt(144) == 12);
assert(round(m.pi, 4) == 3.1416);
assert(py_call(m.pow, [2, 6]) == 64);

let builtins = py_import("builtins");
assert(callable(m.sqrt));

let collections = py_import("collections");
let cnt = collections.Counter(["x", "y", "x", "z", "x"]);
assert(cnt["x"] == 3);
cnt["w"] = 7;
assert(cnt["w"] == 7);
assert(cnt.most_common(1)[0][0] == "w" or cnt.most_common(1)[0][0] == "x");

py_exec("class VyTestBox:\n    def __init__(self, v):\n        self.v = v\n    def bump(self, delta=1):\n        return self.v + delta\n");
let box = py_eval("VyTestBox(10)");
assert(box.v == 10);
box.v = 25;
assert(box.v == 25);
assert(box.bump(delta=7) == 32);
assert(hasattr(box, "v") and getattr(box, "v") == 25);
setattr(box, "label", "py");
assert(box.label == "py");
assert(contains(py_dir(box), "bump"));
assert(repr(box) != "");
"#;
        assert_eq!(run_source(src), Ok(()));
    }

    #[test]
    fn inprocess_cpython_and_zero_copy_numpy_torch_buffers() {
        let src = r#"
let info = py_info();
assert(info["available"]);
assert(info["mode"] == "in-process-capi");
assert(info["zero_copy_buffer"]);

let np = py_import("numpy");
let arr = np.array([[1.5, 2.5, 3.5], [4.5, 5.5, 6.5]], dtype=np.float64);
let t = py_to_tensor(arr);
assert(tensor_shape(t) == [2, 3]);
assert(tensor_values(t) == [1.5, 2.5, 3.5, 4.5, 5.5, 6.5]);

let t2 = tensor_add(t, t);
let back = tensor_to_py(t2);
assert(list(back.shape) == [2, 3]);
assert(float(back.sum()) == 48.0);

let raw_i32 = np.array([[1, 2, 3], [4, 5, 6]], dtype=np.int32).T;
let ti = py_to_tensor(raw_i32);
assert(tensor_shape(ti) == [3, 2]);
assert(tensor_values(ti) == [1, 4, 2, 5, 3, 6]);
assert(py_free(arr));
assert(not py_free(arr));
assert(py_free(back));
assert(py_free(raw_i32));

let torch = py_import("torch");
let th = torch.tensor([[2.0, 4.0], [6.0, 8.0]]).numpy();
let tt = py_to_tensor(th);
assert(tensor_shape(tt) == [2, 2]);
assert(tensor_values(tt) == [2, 4, 6, 8]);
assert(py_free(th));
"#;
        assert_eq!(run_source(src), Ok(()));

        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = env::temp_dir().join(format!("veyra_use_py_{stamp}"));
        fs::create_dir_all(&dir).unwrap();
        let script = dir.join("main.vy");
        fs::write(
            &script,
            r#"use "py:math";
use "py:collections" as col;
assert(math.sqrt(25) == 5);
let c = col.Counter(["a", "b", "a"]);
assert(c["a"] == 2);
"#,
        )
        .unwrap();
        let loaded = load_source(script.to_string_lossy().as_ref()).unwrap();
        assert_eq!(run_source(&loaded), Ok(()));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn aot_full_python_parity_async_generators_dunders_and_py_bridge() {
        let src = r#"
fn make_mul(k) {
    return lambda x: x * k;
}

async fn square_async(n: Int) -> Int {
    return n * n;
}

class AsyncWorker {
    async fn plus(self, x: Int) -> Int {
        return x + 10;
    }
}

fn gen_up(n) {
    let i = 0;
    while i < n {
        yield i * 2;
        i = i + 1;
    }
}

class Box {
    fn init(self, n) {
        self.n = n;
    }
    fn __add__(self, o) {
        return Box(self.n + o.n);
    }
    fn __str__(self) {
        return f"Box({self.n})";
    }
    fn __len__(self) {
        return self.n;
    }
    fn __call__(self, offset=0, **kw) {
        return self.n + offset + kw["bonus"];
    }
}

let t1 = square_async(3);
let t2 = square_async(4);
let async_vals = gather([t1, t2]);
let aw = AsyncWorker();
let aw_res = await aw.plus(5);

let g = gen_up(4);
let g_first = next(g);
let g_rest = collect(g);
let g_sum = sum(x * x for x in range(6) if x % 2 == 0);

let hits = 0;
fn touch(x) {
    hits = hits + 1;
    return x;
}
let chain_ok = (1 < touch(3) < 5) and hits == 1;

let bx = Box(5) + Box(7);
let bx_called = bx(3, bonus=2);
let words = ["bb", "a", "cccc"];
let sorted_words = sorted(words, key=lambda s: len(s), reverse=true);
let dm = divmod(-7, 3);

let m = py_import("math");
let py_root = m.sqrt(81);
let py_val = py_eval("sum([10, 20, 30])");

print("PARITY_OUT", async_vals, aw_res, make_mul(4)(5), (lambda z: z + 1)(41), g_first, g_rest, g_sum, chain_ok, str(bx), len(bx), bx_called, sorted_words, dm, py_root, py_val);
"#;
        assert_eq!(run_source(src), Ok(()));

        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = env::temp_dir().join(format!("veyra_parity_aot_{stamp}"));
        fs::create_dir_all(&dir).unwrap();
        let exe_path = dir.join(if cfg!(target_os = "windows") {
            "parity.exe"
        } else {
            "parity"
        });
        let src_path = dir.join("parity.vy");
        fs::write(&src_path, src).unwrap();

        let built = crate::aot::compile_file_to_native(&src_path, Some(&exe_path)).unwrap();
        let out = std::process::Command::new(&built).output().unwrap();
        assert!(
            out.status.success(),
            "native parity stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        let expected = "PARITY_OUT [9, 16] 15 20 42 0 [2, 4, 6] 20 true Box(12) 12 17 [cccc, bb, a] [-3, 2] 9 60";
        assert!(
            stdout.lines().any(|l| l.trim() == expected),
            "unexpected parity stdout: {stdout}"
        );
        let _ = fs::remove_dir_all(dir);
    }

    fn test_env_cwd_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    struct EnvCwdGuard<'a> {
        _guard: std::sync::MutexGuard<'a, ()>,
        orig_cwd: PathBuf,
        saved_env: Vec<(&'static str, Option<String>)>,
    }

    impl<'a> EnvCwdGuard<'a> {
        fn enter(new_cwd: &Path, vars: &[(&'static str, Option<&str>)]) -> Self {
            let guard = test_env_cwd_lock()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let orig_cwd = env::current_dir().unwrap();
            let mut saved_env = Vec::with_capacity(vars.len());
            for (k, val) in vars {
                saved_env.push((*k, env::var(k).ok()));
                match val {
                    Some(v) => unsafe { env::set_var(k, v) },
                    None => unsafe { env::remove_var(k) },
                }
            }
            env::set_current_dir(new_cwd).unwrap();
            Self {
                _guard: guard,
                orig_cwd,
                saved_env,
            }
        }
    }

    impl Drop for EnvCwdGuard<'_> {
        fn drop(&mut self) {
            let _ = env::set_current_dir(&self.orig_cwd);
            for (k, old) in self.saved_env.iter().rev() {
                match old {
                    Some(v) => unsafe { env::set_var(k, v) },
                    None => unsafe { env::remove_var(k) },
                }
            }
        }
    }

    #[test]
    fn jit_executes_hot_loops_and_self_recursion_natively() {
        assert_eq!(
            crate::jit::jit_available(),
            cfg!(any(target_arch = "x86_64", target_arch = "aarch64"))
        );
        let src = r#"
fn sum_squares(n) {
    let acc = 0.0;
    let i = 1.0;
    while i <= n {
        acc = acc + i * i;
        i = i + 1.0;
    }
    return acc;
}

fn fib_jit(n) {
    if n <= 1.0 {
        return n;
    }
    return fib_jit(n - 1.0) + fib_jit(n - 2.0);
}

fn in_band(x, lo, hi) {
    return x >= lo and x <= hi;
}

fn add_int(a: Int, b: Int) -> Int {
    return a + b;
}

fn non_jit_text(s) {
    return "hi " + s;
}

assert(jit_available() == true);
assert(jit_compile(fib_jit) == true);
assert(jit_compile(non_jit_text) == false);

let k = 0;
let last_sq = 0.0;
let last_add = 0;
let band_hits = 0;
while k < 32 {
    last_sq = sum_squares(20.0);
    last_add = add_int(17, 25);
    if in_band(k, 5, 14) {
        band_hits = band_hits + 1;
    }
    k = k + 1;
}

assert(abs(last_sq - 2870.0) < 0.0001);
assert(last_add == 42);
assert(band_hits == 10);
assert(abs(fib_jit(20.0) - 6765.0) < 0.0001);

let info = jit_info();
assert(info["available"] == true);
assert(info["enabled"] == true);
assert(info["compiled_functions"] >= 3);
assert(info["native_calls"] >= 20);
"#;
        assert_eq!(run_source(src), Ok(()));
    }

    #[test]
    fn jit_inter_calls_list_slices_and_aarch64_encoder() {
        let src = r#"
fn jit_square(x) {
    return x * x;
}

fn jit_hypot2(a, b) {
    return jit_square(a) + jit_square(b);
}

fn jit_is_even(n) {
    if n == 0 {
        return true;
    }
    return jit_is_odd(n - 1);
}

fn jit_is_odd(n) {
    if n == 0 {
        return false;
    }
    return jit_is_even(n - 1);
}

fn jit_dot(a, b) {
    let i = 0;
    let s = 0.0;
    let n = len(a);
    while i < n {
        s = s + a[i] * b[i];
        i = i + 1;
    }
    return s;
}

fn jit_tail_plus_outer(xs) {
    return xs[-1] + jit_hypot2(xs[0], xs[1]);
}

assert(jit_compile(jit_hypot2) == true);
assert(jit_compile(jit_is_even) == true);
assert(jit_compile(jit_dot) == true);
assert(jit_compile(jit_tail_plus_outer) == true);

let before_calls = jit_info()["native_calls"];
let k = 0;
let h = 0.0;
let d = 0.0;
let tail_sum = 0.0;
let ev = false;
let od = false;
let xs = [3.0, 4.0, 5.0, 10.0];
let ys = [2.0, -1.0, 4.0, 0.5];
while k < 24 {
    h = jit_hypot2(3.0, 4.0);
    d = jit_dot(xs, ys);
    tail_sum = jit_tail_plus_outer(xs);
    ev = jit_is_even(12);
    od = jit_is_odd(13);
    k = k + 1;
}
assert(abs(h - 25.0) < 0.0001);
assert(abs(d - 27.0) < 0.0001);
assert(abs(tail_sum - 35.0) < 0.0001);
assert(ev == true);
assert(od == true);
assert(jit_info()["native_calls"] > before_calls + 40);
"#;
        assert_eq!(run_source(src), Ok(()));

        // Out-of-bounds index inside a compiled List-JIT function must cleanly fall back to VM error.
        let oob_src = r#"
fn jit_bad_idx(xs, idx) {
    return xs[idx];
}
assert(jit_compile(jit_bad_idx) == true);
let r = 0.0;
let i = 0;
while i < 12 {
    r = jit_bad_idx([10.0, 20.0], 0);
    i = i + 1;
}
assert(r == 10.0);
let _ = jit_bad_idx([10.0, 20.0], 99);
"#;
        let oob_err = run_source(oob_src).expect_err("expected OOB list index error");
        assert!(
            oob_err.contains("out of range"),
            "unexpected err: {oob_err}"
        );

        // Verify unconditional ARM64 (aarch64) instruction word emitter on inter-call + list indexing.
        let compiled_bc = crate::pipeline::compile_source(src).unwrap();
        let mut fn_map = std::collections::HashMap::new();
        for op in &compiled_bc {
            if let crate::bytecode::Op::Def(fname, params, rty, is_async, body) = op {
                let is_gen = body.iter().any(|o| matches!(o, crate::bytecode::Op::Yield));
                fn_map.insert(
                    fname.clone(),
                    crate::bytecode::FunctionDef {
                        params: std::sync::Arc::new(params.clone()),
                        return_type: rty.clone(),
                        is_async: *is_async,
                        is_generator: is_gen,
                        code: std::sync::Arc::new(body.clone()),
                        captures: std::sync::Arc::new(std::collections::HashMap::new()),
                        capture_types: std::sync::Arc::new(std::collections::HashMap::new()),
                        upvalues: std::sync::Arc::new(std::collections::HashMap::new()),
                        local_count: crate::bytecode::compute_local_count(body, params.len()),
                        param_named: std::sync::Arc::new(vec![true; params.len()]),
                        debug_name: fname.clone(),
                        debug_scope: 0,
                    },
                );
            }
        }
        let arm_words = crate::jit::debug_emit_aarch64(
            "jit_tail_plus_outer",
            fn_map.get("jit_tail_plus_outer").unwrap(),
            &|n| fn_map.get(n).cloned(),
        )
        .unwrap();
        assert_eq!(arm_words[0], 0xA9BF_7BFD); // STP X29, X30, [SP, #-16]!
        assert_eq!(arm_words[1], 0x9100_03FD); // MOV X29, SP
        assert_eq!(arm_words[arm_words.len() - 1], 0xD65F_03C0); // RET
        assert!(arm_words.contains(&0xD63F_0200)); // BLR X16 (inter-function / runtime helper call)
        assert!(arm_words.contains(&0x1E61_2800)); // FADD D0, D0, D1
    }

    fn test_gpu_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    struct GpuStateResetGuard<'a> {
        _guard: std::sync::MutexGuard<'a, ()>,
    }

    impl<'a> GpuStateResetGuard<'a> {
        fn enter() -> Self {
            let guard = test_gpu_lock().lock().unwrap_or_else(|e| e.into_inner());
            let _ = crate::gpu::gpu_set_backend("auto");
            let _ = crate::gpu::gpu_set_precision("f32");
            Self { _guard: guard }
        }
    }

    impl Drop for GpuStateResetGuard<'_> {
        fn drop(&mut self) {
            let _ = crate::gpu::gpu_set_backend("auto");
            let _ = crate::gpu::gpu_set_precision("f32");
        }
    }

    #[test]
    fn gpu_compute_and_tensor_device_parity() {
        let _gpu_guard = GpuStateResetGuard::enter();
        let mut src = String::from(
            r#"
let info = gpu_info();
let devs = gpu_devices();
assert(len(devs) >= 1);

let mm = gpu_matmul([[1.0, 2.0], [3.0, 4.0]], [[5.0, 6.0], [7.0, 8.0]]);
assert(abs(mm[0][0] - 19.0) < 0.0001);
assert(abs(mm[0][1] - 22.0) < 0.0001);
assert(abs(mm[1][0] - 43.0) < 0.0001);
assert(abs(mm[1][1] - 50.0) < 0.0001);

let vadd = gpu_vec_add([1.5, 2.5, 3.5], [0.5, 1.5, 2.5]);
assert(abs(vadd[0] - 2.0) < 0.0001);
assert(abs(vadd[1] - 4.0) < 0.0001);
assert(abs(vadd[2] - 6.0) < 0.0001);

let vsub = gpu_vec_sub([5.0, 3.0], [2.0, 4.0]);
assert(abs(vsub[0] - 3.0) < 0.0001);
assert(abs(vsub[1] + 1.0) < 0.0001);

let vmul = gpu_vec_mul([2.0, 3.0], [4.0, 5.0]);
assert(abs(vmul[0] - 8.0) < 0.0001);
assert(abs(vmul[1] - 15.0) < 0.0001);

let vsc = gpu_scale([2.0, 4.0], 2.5);
assert(abs(vsc[0] - 5.0) < 0.0001);
assert(abs(vsc[1] - 10.0) < 0.0001);

let vrelu = gpu_relu([-2.0, 0.0, 3.5]);
assert(abs(vrelu[0]) < 0.0001);
assert(abs(vrelu[2] - 3.5) < 0.0001);

let vsig = gpu_sigmoid([0.0]);
assert(abs(vsig[0] - 0.5) < 0.0001);

let sm = gpu_softmax([[1.0, 2.0, 3.0]]);
assert(abs(sm[0][0] + sm[0][1] + sm[0][2] - 1.0) < 0.0001);

let ta = tensor_to(tensor([[1.0, 2.0], [3.0, 4.0]]), "gpu");
let tb = tensor_to(tensor([[5.0, -6.0], [7.0, 8.0]]), "gpu");
let tc = tensor_relu(tensor_matmul(ta, tb));
assert(tensor_device(tc) == "gpu");
let vals = tensor_values(tc);
assert(abs(vals[0] - 19.0) < 0.001);
assert(abs(vals[1] - 10.0) < 0.001);
assert(abs(vals[2] - 43.0) < 0.001);
assert(abs(vals[3] - 14.0) < 0.001);
let tc_cpu = tensor_to(tc, "cpu");
assert(tensor_device(tc_cpu) == "cpu");
"#,
        );
        if crate::gpu::gpu_available() {
            src.push_str(
                r#"
let hlsl = "StructuredBuffer<float> bufA;\nStructuredBuffer<float> bufB;\nRWStructuredBuffer<float> outBuf;\n[numthreads(64, 1, 1)]\nvoid main(uint3 id : SV_DispatchThreadID) {\n    if (id.x < 3) { outBuf[id.x] = bufA[id.x] * 3.0 + bufB[id.x]; }\n}\n";
let res = gpu_run_hlsl(hlsl, "main", [3.0, 7.0, 11.0], [4.0, 5.0, 6.0], 3);
assert(abs(res[0] - 13.0) < 0.0001);
assert(abs(res[1] - 26.0) < 0.0001);
assert(abs(res[2] - 39.0) < 0.0001);
let after = gpu_info();
assert(after["available"] == true);
assert(after["dispatches"] >= 1);
assert(after["shaders_compiled"] >= 1);
"#,
            );
        }
        assert_eq!(run_source(&src), Ok(()));
    }

    #[test]
    fn gpu_vulkan_spirv_and_d3d11_f64_kernels() {
        let _gpu_guard = GpuStateResetGuard::enter();
        let backends = crate::gpu::gpu_backends();
        assert!(backends.iter().any(|b| b == "cpu"));

        // Verify SPIR-V generator + Vulkan compute path when Vulkan is available on this host.
        let ewise_words = crate::spirv::spirv_ewise();
        assert!(!ewise_words.is_empty());
        assert_eq!(ewise_words[0], 0x0723_0203);
        let words_literal = ewise_words
            .iter()
            .map(|w| w.to_string())
            .collect::<Vec<_>>()
            .join(", ");

        let has_vulkan = backends.iter().any(|b| b == "vulkan");
        let has_d3d11 = backends.iter().any(|b| b == "d3d11");

        let mut src = String::from(
            r#"
let bs = gpu_backends();
assert(len(bs) >= 1);
assert(gpu_precision() == "f32");
"#,
        );

        if has_vulkan {
            src.push_str(&format!(
                r#"
assert(gpu_set_backend("vulkan") == "vulkan");
let vinfo = gpu_info();
assert(vinfo["available"] == true);
assert(vinfo["backend"] == "vulkan-compute" or vinfo["backend"] == "vulkan-cpu");
assert(contains(str(vinfo["api_version"]), "."));
let before_vk = vinfo["dispatches"];

let mm = gpu_matmul([[1.0, 2.0, 3.0], [4.0, 5.0, 6.0]], [[7.0, 8.0], [9.0, 10.0], [11.0, 12.0]]);
assert(abs(mm[0][0] - 58.0) < 0.001);
assert(abs(mm[0][1] - 64.0) < 0.001);
assert(abs(mm[1][0] - 139.0) < 0.001);
assert(abs(mm[1][1] - 154.0) < 0.001);

let va = gpu_vec_add([1.25, -2.5, 3.5], [2.75, 4.5, -1.5]);
assert(abs(va[0] - 4.0) < 0.0001 and abs(va[1] - 2.0) < 0.0001 and abs(va[2] - 2.0) < 0.0001);
let vs = gpu_vec_sub([5.0, 1.0], [2.0, 3.5]);
assert(abs(vs[0] - 3.0) < 0.0001 and abs(vs[1] + 2.5) < 0.0001);
let vm = gpu_vec_mul([2.5, -3.0], [4.0, 2.0]);
assert(abs(vm[0] - 10.0) < 0.0001 and abs(vm[1] + 6.0) < 0.0001);
let vr = gpu_relu([-3.0, 0.0, 2.25]);
assert(abs(vr[0]) < 0.0001 and abs(vr[2] - 2.25) < 0.0001);
let vsig = gpu_sigmoid([0.0, 2.0]);
assert(abs(vsig[0] - 0.5) < 0.0001 and abs(vsig[1] - 0.880797) < 0.0005);
let vsc = gpu_scale([1.5, -2.0], 3.0);
assert(abs(vsc[0] - 4.5) < 0.0001 and abs(vsc[1] + 6.0) < 0.0001);

let vg = gpu_gelu([-2.0, 0.0, 1.0, 2.0]);
assert(abs(vg[0] + 0.045402) < 0.001);
assert(abs(vg[1]) < 0.0001);
assert(abs(vg[2] - 0.841192) < 0.001);
assert(abs(vg[3] - 1.954598) < 0.001);

let vsm = gpu_softmax([[1.0, 2.0, 3.0], [0.0, 0.0, 0.0]]);
assert(abs(vsm[0][0] - 0.090031) < 0.001 and abs(vsm[0][1] - 0.244728) < 0.001 and abs(vsm[0][2] - 0.665241) < 0.001);
assert(abs(vsm[1][0] - 0.333333) < 0.001);

let vln = gpu_layernorm([[1.0, 2.0, 3.0], [10.0, 10.0, 10.0]]);
assert(abs(vln[0][0] + 1.224735) < 0.002 and abs(vln[0][1]) < 0.001 and abs(vln[0][2] - 1.224735) < 0.002);
assert(abs(vln[1][0]) < 0.001 and abs(vln[1][2]) < 0.001);

let spv_words = [{words_literal}];
let spv_out = gpu_run_spirv(spv_words, [1.5, -2.0, 3.0, 0.5], [4.0, 3.0, -2.0, 8.0], 4, [4, 2, 0, 0], 1, 1, 1);
assert(abs(spv_out[0] - 6.0) < 0.0001);
assert(abs(spv_out[1] + 6.0) < 0.0001);
assert(abs(spv_out[2] + 6.0) < 0.0001);
assert(abs(spv_out[3] - 4.0) < 0.0001);

let after_vk = gpu_info();
assert(after_vk["dispatches"] > before_vk);
"#,
            ));
        }

        if has_d3d11 {
            src.push_str(
                r#"
assert(gpu_set_backend("d3d11") == "d3d11");
let dln = gpu_layernorm([[1.0, 2.0, 3.0]]);
assert(abs(dln[0][0] + 1.224735) < 0.002 and abs(dln[0][2] - 1.224735) < 0.002);
let dg = gpu_gelu([0.0, 1.0]);
assert(abs(dg[0]) < 0.0001 and abs(dg[1] - 0.841192) < 0.001);
"#,
            );
        }

        // Test f64 precision mode (uses hardware D3D11 doubles if supported, otherwise CPU f64)
        src.push_str(
            r#"
assert(gpu_set_precision("f64") == "f64");
assert(gpu_precision() == "f64");
let hi_prec = gpu_vec_add([100000000.0, 1.0], [0.125, 2.5]);
assert(hi_prec[0] == 100000000.125);
assert(hi_prec[1] == 3.5);
let hi_mm = gpu_matmul([[100000000.0, 1.0]], [[1.0], [0.125]]);
assert(hi_mm[0][0] == 100000000.125);
assert(gpu_set_precision("f32") == "f32");
assert(gpu_set_backend("auto") == "auto");
"#,
        );
        assert_eq!(run_source(&src), Ok(()));

        // Also test vygpu package wrapper end-to-end via run_file
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let vygpu_vy = repo_root
            .join("packages")
            .join("vygpu")
            .join("vygpu.vy")
            .to_string_lossy()
            .replace('\\', "/");
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let tmp_vy = env::temp_dir().join(format!("veyra_vygpu_pkg_{stamp}.vy"));
        let pkg_src = format!(
            r#"
use "{vygpu_vy}";
let b = backends();
assert(len(b) >= 1);
let g = gelu([0.0, 1.0]);
assert(abs(g[0]) < 0.0001 and abs(g[1] - 0.841192) < 0.001);
let ln = layernorm([[2.0, 4.0, 6.0]]);
assert(abs(ln[0][1]) < 0.001);
assert(precision() == "f32");
"#
        );
        fs::write(&tmp_vy, pkg_src).unwrap();
        let loaded = load_source(tmp_vy.to_string_lossy().as_ref());
        let _ = fs::remove_file(&tmp_vy);
        let res = loaded.and_then(|s| run_source(&s));
        assert_eq!(res, Ok(()));
    }

    #[test]
    fn native_veyra_plugin_abi_and_package_import() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = env::temp_dir().join(format!("veyra_plugin_test_{stamp}"));
        let proj = dir.join("proj");
        let pkg_dir = proj.join("vendor").join("nmath");
        fs::create_dir_all(proj.join("src")).unwrap();
        fs::create_dir_all(&pkg_dir).unwrap();

        let plugin_rs = r#"
use std::ffi::{CStr, CString};
use std::os::raw::c_char;

static MANIFEST: &[u8] = b"{\"abi_version\":1,\"name\":\"nmath\",\"version\":\"0.1.0\",\"functions\":[{\"name\":\"vadd_f64\",\"symbol\":\"vadd_f64\",\"kind\":\"f64\",\"arity\":2},{\"name\":\"vscale_slice\",\"symbol\":\"vscale_slice\",\"kind\":\"f64_slice\",\"arity\":1},{\"name\":\"vgreet_json\",\"symbol\":\"vgreet_json\",\"kind\":\"json\",\"arity\":1}]}\0";

#[no_mangle]
pub extern "C" fn veyra_plugin_abi_version() -> u32 {
    1
}

#[no_mangle]
pub extern "C" fn veyra_plugin_manifest() -> *const c_char {
    MANIFEST.as_ptr() as *const c_char
}

#[no_mangle]
pub unsafe extern "C" fn veyra_plugin_free(ptr: *mut c_char) {
    if !ptr.is_null() {
        drop(unsafe { CString::from_raw(ptr) });
    }
}

#[no_mangle]
pub extern "C" fn vadd_f64(a: f64, b: f64) -> f64 {
    a + b
}

#[no_mangle]
pub unsafe extern "C" fn vscale_slice(inp: *const f64, len: usize, out: *mut f64) -> usize {
    for i in 0..len {
        unsafe { *out.add(i) = *inp.add(i) * 2.5 };
    }
    len
}

#[no_mangle]
pub unsafe extern "C" fn vgreet_json(args_ptr: *const c_char) -> *mut c_char {
    let raw = if args_ptr.is_null() {
        "[]"
    } else {
        unsafe { CStr::from_ptr(args_ptr) }.to_str().unwrap_or("[]")
    };
    let reply = format!("{{\"ok\":\"plugin-ok:{}\"}}", raw.len());
    CString::new(reply).unwrap().into_raw()
}
"#;
        let rs_path = dir.join("nmath.rs");
        let ext = crate::plugin::platform_shared_ext();
        let dll_path = pkg_dir.join(format!("nmath.{ext}"));
        fs::write(&rs_path, plugin_rs).unwrap();
        let built = crate::plugin::build_rust_plugin(&rs_path, &dll_path).unwrap();
        assert!(built.is_file());

        let direct_src = format!(
            r#"
let p = plugin_load("{}");
assert(p["name"] == "nmath");
assert(p["abi_version"] == 1);
let s = plugin_call(p["id"], "vadd_f64", [19.5, 22.5]);
assert(abs(s - 42.0) < 0.00001);
let sc = plugin_call(p["id"], "vscale_slice", [[2.0, 4.0, 6.0]]);
assert(abs(sc[0] - 5.0) < 0.00001);
assert(abs(sc[2] - 15.0) < 0.00001);
let gr = plugin_call(p["id"], "vgreet_json", ["veyra"]);
assert(starts_with(gr, "plugin-ok:"));
assert(len(plugin_list()) >= 1);
"#,
            built.to_string_lossy().replace('\\', "\\\\")
        );
        assert_eq!(run_source(&direct_src), Ok(()));

        fs::write(
            proj.join("veyra.json"),
            r#"{"name":"plugapp","version":"0.1.0","entry":"src/main.vy","packages":{"nmath":{"version":"0.1.0","entry":"nmath.vy"}}}"#,
        )
        .unwrap();
        fs::write(
            pkg_dir.join("veyra.json"),
            r#"{"name":"nmath","version":"0.1.0","native":{"library":"nmath"}}"#,
        )
        .unwrap();
        let main_vy = proj.join("src/main.vy");
        fs::write(
            &main_vy,
            r#"
use "nmath";
assert(abs(nmath.vadd_f64(11.0, 31.0) - 42.0) < 0.00001);
let sc = nmath.vscale_slice([4.0, 8.0]);
assert(abs(sc[0] - 10.0) < 0.00001);
assert(abs(sc[1] - 20.0) < 0.00001);
let gr = nmath.vgreet_json("native");
assert(starts_with(gr, "plugin-ok:"));
"#,
        )
        .unwrap();

        let _guard = EnvCwdGuard::enter(&proj, &[]);
        let loaded = load_source(main_vy.to_string_lossy().as_ref()).unwrap();
        assert_eq!(run_source(&loaded), Ok(()));
        drop(_guard);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn sovereign_registry_signing_semver_pack_tree_and_http_publish() {
        assert!(crate::registry::semver_matches("1.2.5", "^1.2.3"));
        assert!(!crate::registry::semver_matches("2.0.0", "^1.2.3"));
        assert!(crate::registry::semver_matches("1.2.9", "~1.2.3"));
        assert!(!crate::registry::semver_matches("1.3.0", "~1.2.3"));
        assert!(crate::registry::semver_matches("1.5.0", ">=1.2.0 <2.0.0"));
        assert!(crate::registry::semver_matches("3.4.5", "*"));

        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!("veyra_reg_test_{stamp}"));
        let home = root.join("home");
        let pkg_src = root.join("vy_pkg_a");
        let consumer = root.join("consumer");
        let unpacked = root.join("unpacked");
        fs::create_dir_all(home.join("registry")).unwrap();
        fs::create_dir_all(home.join("packages")).unwrap();
        fs::create_dir_all(&pkg_src).unwrap();
        fs::create_dir_all(consumer.join("src")).unwrap();
        fs::write(
            home.join("registry/index.json"),
            r#"{"registry_version":1,"packages":{}}"#,
        )
        .unwrap();
        fs::write(
            pkg_src.join("veyra.json"),
            r#"{"name":"vy_pkg_a","version":"1.2.4","entry":"vy_pkg_a.vy"}"#,
        )
        .unwrap();
        fs::write(
            pkg_src.join("vy_pkg_a.vy"),
            "fn answer() -> Int { return 42; }\n",
        )
        .unwrap();
        fs::write(
            consumer.join("veyra.json"),
            r#"{"name":"consumer","version":"0.1.0","entry":"src/main.vy"}"#,
        )
        .unwrap();
        fs::write(
            consumer.join("src/main.vy"),
            "use \"vy_pkg_a\";\nassert(vy_pkg_a.answer() == 42);\n",
        )
        .unwrap();

        let home_str = home.to_string_lossy().to_string();
        let _guard = EnvCwdGuard::enter(
            &pkg_src,
            &[
                ("VEYRA_HOME", Some(&home_str)),
                ("VEYRA_SIGNING_KEY", None),
                ("VEYRA_REGISTRY_URL", None),
            ],
        );

        crate::registry::pkg_keygen(Some("ci_key")).unwrap();
        assert!(home.join("keys/ci_key.json").is_file());

        crate::registry::pkg_sign(Some(&pkg_src), Some("ci_key")).unwrap();
        assert!(pkg_src.join("veyra.sig.json").is_file());
        crate::registry::pkg_verify(Some(pkg_src.to_string_lossy().as_ref())).unwrap();

        // Tamper detection
        fs::write(
            pkg_src.join("vy_pkg_a.vy"),
            "fn answer() -> Int { return 99; }\n",
        )
        .unwrap();
        assert!(crate::registry::pkg_verify(Some(pkg_src.to_string_lossy().as_ref())).is_err());

        // Restore & re-sign
        fs::write(
            pkg_src.join("vy_pkg_a.vy"),
            "fn answer() -> Int { return 42; }\n",
        )
        .unwrap();
        crate::registry::pkg_sign(Some(&pkg_src), Some("ci_key")).unwrap();

        // Pack & verify & unpack
        let bundle = root.join("vy_pkg_a.vypkg");
        crate::registry::pkg_pack(Some(&pkg_src), Some(&bundle)).unwrap();
        crate::registry::pkg_verify(Some(bundle.to_string_lossy().as_ref())).unwrap();
        crate::registry::pkg_unpack(&bundle, Some(&unpacked)).unwrap();
        assert_eq!(
            fs::read_to_string(unpacked.join("vy_pkg_a.vy")).unwrap(),
            "fn answer() -> Int { return 42; }\n"
        );

        #[cfg(feature = "native-services")]
        {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            drop(listener);
            let srv_home = home.clone();
            std::thread::spawn(move || {
                let _ = crate::server::serve_registry(&srv_home, port);
            });
            let url = format!("http://127.0.0.1:{port}");
            let mut ready = false;
            for _ in 0..40 {
                if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                    ready = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            assert!(ready, "registry server failed to listen on {url}");

            unsafe { env::set_var("VEYRA_REGISTRY_URL", &url) };
            crate::registry::pkg_publish().unwrap();

            let body = reqwest::blocking::get(format!("{url}/index.json"))
                .unwrap()
                .text()
                .unwrap();
            let idx: JsonValue = serde_json::from_str(&body).unwrap();
            assert_eq!(
                idx["packages"]["vy_pkg_a"]["version"].as_str(),
                Some("1.2.4")
            );
            assert!(idx["packages"]["vy_pkg_a"]["sha256"].as_str().is_some());

            env::set_current_dir(&consumer).unwrap();
            crate::registry::pkg_add("vy_pkg_a").unwrap();
            crate::registry::pkg_info("vy_pkg_a").unwrap();
            crate::registry::pkg_tree().unwrap();
            crate::registry::pkg_audit().unwrap();

            let lock_txt = fs::read_to_string(consumer.join("veyra.lock")).unwrap();
            let lock: JsonValue = serde_json::from_str(&lock_txt).unwrap();
            assert_eq!(lock["format"].as_u64(), Some(3));
            assert_eq!(lock["packages"]["vy_pkg_a"]["signed"].as_bool(), Some(true));
        }

        drop(_guard);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn all_8_sovereign_packages_end_to_end() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        let proj = env::temp_dir().join(format!("veyra_8pkgs_test_{stamp}"));
        fs::create_dir_all(proj.join("src")).unwrap();
        fs::write(
            proj.join("veyra.json"),
            r#"{"name":"eightpkgs","version":"0.1.0","entry":"src/main.vy"}"#,
        )
        .unwrap();

        let home_str = workspace_root.to_string_lossy().to_string();
        let _guard = EnvCwdGuard::enter(
            &proj,
            &[
                ("VEYRA_HOME", Some(&home_str)),
                ("VEYRA_REGISTRY_URL", None),
            ],
        );

        for name in [
            "vyjit",
            "vygpu",
            "vyplugin",
            "vycache",
            "vydiff",
            "vytemplate",
            "vyargparse",
            "vydecimal",
        ] {
            crate::registry::pkg_add(name).unwrap();
        }

        let main_vy = proj.join("src/main.vy");
        fs::write(
            &main_vy,
            r#"
use "vyjit";
use "vygpu";
use "vyplugin";
use "vycache";
use "vydiff";
use "vytemplate";
use "vyargparse";
use "vydecimal";

fn sq_loop(n) {
    let s = 0.0;
    let i = 0.0;
    while i < n {
        s = s + i;
        i = i + 1.0;
    }
    return s;
}
assert(vyjit.available() == cfg_is_x64());
let jr = vyjit.bench(sq_loop, 10.0, 20);
assert(abs(jr["last"] - 45.0) < 0.0001);

let gm = vygpu.matmul([[1.0, 2.0], [3.0, 4.0]], [[2.0, 0.0], [1.0, 2.0]]);
assert(abs(gm[0][0] - 4.0) < 0.001);
assert(len(vygpu.backends()) >= 1);
assert(abs(vygpu.gelu([1.0])[0] - 0.841192) < 0.001);
assert(abs(vygpu.layernorm([[1.0, 2.0, 3.0]])[0][1]) < 0.001);
assert(vyplugin.abi_version() == 1);

let c = vycache.create(2);
c = vycache.put(c, "k1", 100);
c = vycache.put(c, "k2", 200);
c = vycache.put(c, "k3", 300);
assert(vycache.get(c, "k1", null) == null);
assert(vycache.get(c, "k3", null) == 300);

let ops = vydiff.diff_lines("a\nb\nc", "a\nx\nc\nd");
assert(len(ops) >= 4);
assert(vydiff.ratio("same\nline", "same\nline") == 1.0);

let rendered = vytemplate.render("Hi {{user}}{{#if ok}}!{{/if}}", {"user": "<V>", "ok": true});
assert(rendered == "Hi &lt;V&gt;!");

let pspec = vyargparse.spec("cli", "Test");
pspec = vyargparse.add_flag(pspec, "verbose", "v", "Verbose");
pspec = vyargparse.add_option(pspec, "port", "p", "8080", "Port");
let parsed = vyargparse.parse(pspec, ["-v", "-p", "9090", "run.vy"]);
assert(parsed["values"]["verbose"] == true);
assert(parsed["values"]["port"] == "9090");
assert(parsed["positionals"][0] == "run.vy");

let d1 = vydecimal.from_str("0.1", 2);
let d2 = vydecimal.from_str("0.2", 2);
assert(vydecimal.format(vydecimal.add(d1, d2)) == "0.30");
assert(vydecimal.format(vydecimal.mul(vydecimal.from_str("1.25", 2), vydecimal.from_str("0.40", 2))) == "0.50");
"#,
        )
        .unwrap();

        // Normalize cfg_is_x64() without inventing a builtin!
        let code = fs::read_to_string(&main_vy).unwrap().replace(
            "cfg_is_x64()",
            if cfg!(target_arch = "x86_64") {
                "true"
            } else {
                "false"
            },
        );
        fs::write(&main_vy, &code).unwrap();

        let loaded = load_source(main_vy.to_string_lossy().as_ref()).unwrap();
        assert_eq!(run_source(&loaded), Ok(()));
        drop(_guard);

        let _ = fs::remove_dir_all(proj);
    }

    #[test]
    fn bigint_decimal_complex_and_fft_parity() {
        let src = r#"
let p256 = big_pow(2, 256);
assert(p256 == "115792089237316195423570985008687907853269984665640564039457584007913129639936");
assert(big_bits(p256) == 257);
assert(big_bits(big_sub(p256, 1)) == 256);

let a = big_add(p256, "12345678901234567890123456789");
let d = bigint("0xdeadbeefcafebabe123456789");
let qr = big_divmod(a, d);
assert(big_add(big_mul(qr[0], d), qr[1]) == a);
assert(big_cmp(qr[1], d) < 0);
assert(big_gcd(big_mul(d, "97"), big_mul(d, "101")) == d);
assert(big_powmod("2", "256", "9999999967") == big_rem(p256, "9999999967"));

assert(dec_add(dec("0.1"), dec("0.2")) == dec("0.3"));
assert(dec_mul("12345678901234567890.12345", "100000") == "1234567890123456789012345");
let third_100 = dec_div("1", "3", 100);
assert(len(third_100) == 102);
assert(starts_with(third_100, "0.33333333333333333333"));
assert(ends_with(third_100, "33333333333333333333"));
let sq2 = dec_sqrt("2", 50);
assert(sq2 == "1.41421356237309504880168872420969807856967187537695");
assert(dec_quantize("0.3", 4) == "0.3000");
assert(dec_round("3.14159", 2) == "3.14");
assert(dec_cmp("1e-20", "0.00000000000000000002") < 0);

let z1 = complex(3, 4);
let z2 = complex(1, -2);
assert(cabs(z1) == 5.0);
let zp = cmul(z1, z2);
assert(zp[0] == 11.0 and zp[1] == -2.0);
let zq = cdiv(zp, z2);
assert(abs(zq[0] - 3.0) < 0.000000001 and abs(zq[1] - 4.0) < 0.000000001);

let f4 = fft([1, 0, -1, 0]);
assert(abs(f4[0][0]) < 0.000000001 and abs(f4[1][0] - 2.0) < 0.000000001 and abs(f4[3][0] - 2.0) < 0.000000001);
let r4 = ifft(f4);
assert(abs(r4[0][0] - 1.0) < 0.000000001 and abs(r4[2][0] + 1.0) < 0.000000001);

// Prime length N=7 Bluestein FFT + IFFT exact round-trip
let sig7 = [1.5, -2.25, 3.0, 0.5, -1.0, 4.25, -0.75];
let r7 = ifft(fft(sig7));
let i = 0;
while i < len(sig7) {
    assert(abs(r7[i][0] - sig7[i]) < 0.000000001);
    assert(abs(r7[i][1]) < 0.000000001);
    i = i + 1;
}
"#;
        assert_eq!(run_source(src), Ok(()));

        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = env::temp_dir().join(format!("veyra_bigint_aot_{stamp}"));
        fs::create_dir_all(&dir).unwrap();
        let exe_path = dir.join(if cfg!(target_os = "windows") {
            "bigint_aot.exe"
        } else {
            "bigint_aot"
        });
        crate::aot::compile_source_to_native(src, &exe_path).unwrap();
        let out = std::process::Command::new(&exe_path).output().unwrap();
        assert!(
            out.status.success(),
            "native bigint AOT stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn oop_mro_super_descriptors_and_metaclasses_parity() {
        let src = r#"
// 1. C3 MRO + Cooperative super() + Diamond Inheritance
class A {
    fn tag(self) {
        return ["A"];
    }
}
class B(A) {
    fn tag(self) {
        return ["B"] + super().tag();
    }
}
class C(A) {
    fn tag(self) {
        return ["C"] + super().tag();
    }
}
class D(B, C) {
    fn tag(self) {
        return ["D"] + super().tag();
    }
}
let d = D();
assert(d.tag() == ["D", "B", "C", "A"]);
assert(isinstance(d, D) and isinstance(d, A) and isinstance(d, [int, C]));
assert(issubclass(D, A) and issubclass(D, object));
assert(type(d) == D and type(D) == type and type(42) == int);
let m = mro(D);
assert(len(m) == 5 and m[0] == D and m[1] == B and m[2] == C and m[3] == A and m[4] == object);
assert(D.__mro__ == m and D.__name__ == "D");

// Legacy single-parent ':' inheritance still supports super() & MRO
class LegacySub: C {
    fn tag(self) {
        return ["L"] + super().tag();
    }
}
assert(LegacySub().tag() == ["L", "C", "A"]);

// 2. @property + @x.setter + @x.deleter + @staticmethod + @classmethod
class Temp {
    scale = 1.8;
    fn __init__(self, c) {
        self._c = c;
        self._deleted = false;
    }
    @property
    fn c(self) {
        return self._c;
    }
    @c.setter
    fn c(self, value) {
        self._c = value;
    }
    @c.deleter
    fn c(self) {
        self._deleted = true;
        self._c = 0;
    }
    @staticmethod
    fn freezing() {
        return 0;
    }
    @classmethod
    fn from_fahrenheit(cls, f) {
        return cls((f - 32) / cls.scale);
    }
}
class KelvinTemp(Temp) {
    scale = 1.8;
}
let t = Temp(25);
assert(t.c == 25);
t.c = 100;
assert(t.c == 100);
delattr(t, "c");
assert(t._deleted == true and t.c == 0);
assert(Temp.freezing() == 0 and t.freezing() == 0);
let kt = KelvinTemp.from_fahrenheit(212);
assert(isinstance(kt, KelvinTemp) and abs(kt.c - 100) < 0.000001);

// 3. Custom Data Descriptor Protocol + Non-Data Descriptor Precedence
class Positive {
    fn __init__(self, slot) {
        self.slot = slot;
    }
    fn __get__(self, inst, owner) {
        if inst == null {
            return self;
        }
        return getattr(inst, self.slot, 0);
    }
    fn __set__(self, inst, val) {
        if val < 0 {
            throw "must be non-negative";
        }
        setattr(inst, self.slot, val);
    }
}
class NonData {
    fn __get__(self, inst, owner) {
        return "from_nondata";
    }
}
class Account {
    balance = Positive("_balance");
    label = NonData();
    fn __init__(self, bal) {
        self.balance = bal;
    }
}
let acc = Account(50);
assert(acc.balance == 50);
acc.balance = 120;
assert(acc.balance == 120);
let caught = false;
try {
    acc.balance = -5;
} catch e {
    caught = true;
}
assert(caught);
assert(acc.label == "from_nondata");
acc.label = "custom";
assert(acc.label == "custom");

// 4. __getattr__, __setattr__ (with super().__setattr__), __new__
class DynamicRecord {
    fn __new__(cls, *args) {
        let inst = super().__new__(cls);
        inst.created_by_new = true;
        return inst;
    }
    fn __init__(self, name) {
        self.name = name;
    }
    fn __setattr__(self, k, v) {
        super().__setattr__(k, v);
        super().__setattr__("_last_set", k);
    }
    fn __getattr__(self, name) {
        return "missing:" + name;
    }
}
let rec = DynamicRecord("alpha");
assert(rec.created_by_new == true and rec.name == "alpha" and rec._last_set == "name");
assert(rec.anything == "missing:anything");

// 5. Stacked Decorators & Dynamic type(name, bases, ns)
fn add_two(f) {
    fn w(x) {
        return f(x) + 2;
    }
    return w;
}
fn mul_three(f) {
    fn w(x) {
        return f(x) * 3;
    }
    return w;
}
@add_two
@mul_three
fn base_op(x) {
    return x + 1;
}
// Order: add_two(mul_three(base_op))(4) = ((4+1)*3)+2 = 17
assert(base_op(4) == 17);

fn dyn_init(self, x) {
    self.x = x;
}
fn dyn_double(self) {
    return self.x * 2;
}
let DynBox = type("DynBox", [object], {"__init__": dyn_init, "double": dyn_double, "kind": "dynamic"});
let db = DynBox(21);
assert(isinstance(db, DynBox) and db.double() == 42 and DynBox.kind == "dynamic");

// 6. Custom Metaclass (__new__, __init__, __call__) + Class Decorator
let cls_reg = [];
fn register_cls(cls) {
    cls.decorated_flag = true;
    cls_reg.push(cls.__name__);
    return cls;
}
class TrackMeta(type) {
    fn __new__(mcs, name, bases, ns) {
        let mod_ns = {};
        for k in ns {
            mod_ns[k] = ns[k];
        }
        mod_ns["META_NAME"] = "meta_" + name;
        return super().__new__(mcs, name, bases, mod_ns);
    }
    fn __init__(cls, name, bases, ns) {
        cls.INITED = true;
    }
    fn __call__(cls, *args) {
        let inst = super().__call__(*args);
        inst.meta_tagged = true;
        return inst;
    }
}
@register_cls
class Service(metaclass=TrackMeta) {
    fn __init__(self, port) {
        self.port = port;
    }
}
assert(Service.META_NAME == "meta_Service" and Service.INITED == true and Service.decorated_flag == true);
assert(cls_reg == ["Service"]);
assert(isinstance(Service, TrackMeta) and isinstance(Service, type));
let svc = Service(8080);
assert(svc.port == 8080 and svc.meta_tagged == true and type(svc) == Service);
"#;
        assert_eq!(run_source(src), Ok(()));

        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = env::temp_dir().join(format!("veyra_oop_aot_{stamp}"));
        fs::create_dir_all(&dir).unwrap();
        let exe_path = dir.join(if cfg!(target_os = "windows") {
            "oop_aot.exe"
        } else {
            "oop_aot"
        });
        crate::aot::compile_source_to_native(src, &exe_path).unwrap();
        let out = std::process::Command::new(&exe_path).output().unwrap();
        assert!(
            out.status.success(),
            "native OOP AOT stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let _ = fs::remove_dir_all(&dir);

        // Inconsistent C3 MRO must fail cleanly in both VM and AOT binary execution
        let bad_mro = r#"
class X {}
class Y {}
class Z1(X, Y) {}
class Z2(Y, X) {}
class Bad(Z1, Z2) {}
"#;
        let vm_err = run_source(bad_mro).unwrap_err();
        assert!(vm_err.contains("MRO"), "expected MRO error, got: {vm_err}");

        let dir2 = env::temp_dir().join(format!("veyra_bad_mro_aot_{stamp}"));
        fs::create_dir_all(&dir2).unwrap();
        let exe2 = dir2.join(if cfg!(target_os = "windows") {
            "bad_mro.exe"
        } else {
            "bad_mro"
        });
        crate::aot::compile_source_to_native(bad_mro, &exe2).unwrap();
        let out2 = std::process::Command::new(&exe2).output().unwrap();
        assert!(
            !out2.status.success(),
            "expected inconsistent MRO binary to fail"
        );
        let stderr2 = String::from_utf8_lossy(&out2.stderr);
        assert!(
            stderr2.contains("MRO"),
            "expected AOT MRO error, got: {stderr2}"
        );
        let _ = fs::remove_dir_all(&dir2);
    }

    #[test]
    fn match_structural_patterns_parity() {
        let src = r#"
class Colors {
    RED = "red";
    GREEN = "green";
}

class Point {
    fn init(self, x, y) {
        self.x = x;
        self.y = y;
    }

    @property
    fn mag2(self) {
        return self.x * self.x + self.y * self.y;
    }
}

class Reordered {
    __match_args__ = ["b", "a"];
    fn init(self, a, b) {
        self.a = a;
        self.b = b;
    }
}

fn classify(item) {
    match item {
        case 1 | 2 | 3 {
            return "small:" + str(item);
        }
        case Colors.RED {
            return "const:red";
        }
        case [1, x] | [2, x] {
            return "orseq:" + str(x);
        }
        case [first, *mid, last] if len(mid) > 0 {
            return "seq:" + str(first) + ":" + str(len(mid)) + ":" + str(last);
        }
        case [a, b] as pair {
            return "pair:" + str(pair);
        }
        case {"op": "add", "args": [a, b]} {
            return "add:" + str(a + b);
        }
        case {"op": op, **rest} {
            return "op:" + str(op) + ":" + str(rest["n"]);
        }
        case Point(0, y=py) if py > 0 {
            return "ypos:" + str(py);
        }
        case Point(px, py, mag2=m2) {
            return "pt:" + str(px) + "," + str(py) + ":" + str(m2);
        }
        case Reordered(first_b, second_a) {
            return "re:" + str(first_b) + "," + str(second_a);
        }
        case int(n) if n > 100 {
            return "big_int:" + str(n);
        }
        case str(s) {
            return "str:" + s;
        }
        case _ if false {
            return "never";
        }
        case _ {
            return "fallback";
        }
    }
}

assert(classify(2) == "small:2");
assert(classify("red") == "const:red");
assert(classify([2, 42]) == "orseq:42");
assert(classify([10, 20, 30, 40]) == "seq:10:2:40");
assert(classify([7, 8]) == "pair:[7, 8]");
assert(classify({"op": "add", "args": [3, 9], "extra": true}) == "add:12");
assert(classify({"op": "mul", "n": 5, "k": 1}) == "op:mul:5");
assert(classify(Point(0, 9)) == "ypos:9");
assert(classify(Point(3, 4)) == "pt:3,4:25");
assert(classify(Reordered(10, 20)) == "re:20,10");
assert(classify(250) == "big_int:250");
assert(classify(50) == "fallback");
assert(classify("hello") == "str:hello");

// Failed sub-patterns must not leak/commit partial captures into later branches
let outside = 999;
match [1, 2] {
    case [outside, 99] { assert(false); }
    case _ {}
}
assert(outside == 999);

// Top-level structural match & negative integer literals
let top_res = "";
match {"k": -5} {
    case {"k": -5} { top_res = "neg5"; }
    case _ { top_res = "no"; }
}
assert(top_res == "neg5");
"#;

        assert_eq!(run_source(src), Ok(()));

        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = env::temp_dir().join(format!("veyra_match_aot_{stamp}"));
        fs::create_dir_all(&dir).unwrap();
        let exe_path = dir.join(if cfg!(target_os = "windows") {
            "match_aot.exe"
        } else {
            "match_aot"
        });
        crate::aot::compile_source_to_native(src, &exe_path).unwrap();
        let out = std::process::Command::new(&exe_path).output().unwrap();
        assert!(
            out.status.success(),
            "native match AOT stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn new_6_sovereign_packages_and_vydecimal_end_to_end() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        let reg_txt =
            fs::read_to_string(workspace_root.join("registry").join("index.json")).unwrap();
        let reg_json: JsonValue = serde_json::from_str(&reg_txt).unwrap();
        assert!(
            reg_json["packages"].as_object().unwrap().len() >= 74,
            "expected at least 74 registered packages"
        );

        let proj = env::temp_dir().join(format!("veyra_new6pkgs_{stamp}"));
        fs::create_dir_all(proj.join("src")).unwrap();
        fs::write(
            proj.join("veyra.json"),
            r#"{"name":"new6pkgs","version":"0.1.0","entry":"src/main.vy"}"#,
        )
        .unwrap();

        let home_str = workspace_root.to_string_lossy().to_string();
        let _guard = EnvCwdGuard::enter(
            &proj,
            &[
                ("VEYRA_HOME", Some(&home_str)),
                ("VEYRA_REGISTRY_URL", None),
            ],
        );

        for name in [
            "vydecimal",
            "vycomplex",
            "vyitertools",
            "vyfunctools",
            "vytyping",
            "vygraph",
            "vysemver",
        ] {
            crate::registry::pkg_add(name).unwrap();
        }

        let main_vy = proj.join("src/main.vy");
        fs::write(
            &main_vy,
            r#"
use "vydecimal";
use "vycomplex";
use "vyitertools";
use "vyfunctools";
use "vytyping";
use "vygraph";
use "vysemver";

// 1. Upgraded vydecimal (arbitrary precision + Decimal OOP class)
let da = vydecimal.parse("0.1");
let db = vydecimal.parse("0.2");
assert(vydecimal.format(vydecimal.add(da, db)) == "0.3");
let one_third = vydecimal.div(vydecimal.parse("1"), vydecimal.parse("3"), 30);
assert(starts_with(vydecimal.format(one_third), "0.33333333333333333333"));
let sq2 = vydecimal.sqrt(vydecimal.parse("2"), 20);
assert(starts_with(vydecimal.format(sq2), "1.414213562373095"));
let d_obj = vydecimal.Decimal("1.25") + vydecimal.Decimal("2.75");
assert(d_obj.str() == "4");

// 2. vycomplex
let z1 = vycomplex.make(3, 4);
let z2 = vycomplex.Complex(1, -2);
let z3 = vycomplex.Complex(2, 5);
let zsum = z2 + z3;
assert(zsum.__str__() == "3+3j");
assert(vycomplex.abs_val(z1) == 5.0);
assert(vycomplex.pow_int(vycomplex.make(0, 1), 2) == [-1.0, 0.0]);

// 3. vyitertools
assert(vyitertools.chain([[1, 2], [3], [4, 5]]) == [1, 2, 3, 4, 5]);
assert(vyitertools.batched([1, 2, 3, 4, 5], 2) == [[1, 2], [3, 4], [5]]);
assert(vyitertools.pairwise([10, 20, 30]) == [[10, 20], [20, 30]]);
assert(vyitertools.accumulate([1, 2, 3, 4], fn(a, b) { return a + b; }) == [1, 3, 6, 10]);
assert(len(vyitertools.combinations([1, 2, 3, 4], 2)) == 6);
assert(len(vyitertools.permutations([1, 2, 3], 2)) == 6);

// 4. vyfunctools
let inc = fn(x) { return x + 1; };
let dbl = fn(x) { return x * 2; };
let f_comp = vyfunctools.compose(dbl, inc);
assert(f_comp(4) == 10);
assert(vyfunctools.pipe(3, [inc, dbl, inc]) == 9);
assert(vyfunctools.reduce(fn(a, b) { return a + b; }, [1, 2, 3, 4], 10) == 20);
let add3 = fn(a, b, c) { return a + b + c; };
let p5 = vyfunctools.partial(add3, [5]);
assert(p5(10, 20) == 35);
let calls = 0;
let fib_slow = fn(n) {
    calls = calls + 1;
    return n * n;
};
let fib_m = vyfunctools.memoize(fib_slow);
assert(fib_m(9) == 81 and fib_m(9) == 81 and calls == 1);
let lru = vyfunctools.lru_cache(2, fib_slow);
assert(lru(2) == 4 and lru(3) == 9 and lru(2) == 4 and calls == 3);

// 5. vytyping
let user_schema = vytyping.typed_dict({
    "name": "str",
    "age": vytyping.optional("int"),
    "roles": vytyping.list_of(vytyping.literal_of(["admin", "user"]))
});
assert(vytyping.assert_schema({"name": "Ada", "age": 36, "roles": ["admin"]}, user_schema));
assert(not vytyping.matches({"name": "Ada", "roles": ["guest"]}, user_schema));

// 6. vygraph
let g = vygraph.new_graph(true);
g = vygraph.add_edge(g, "a", "b", 2);
g = vygraph.add_edge(g, "a", "c", 5);
g = vygraph.add_edge(g, "b", "c", 1);
g = vygraph.add_edge(g, "c", "d", 3);
assert(vygraph.toposort(g) == ["a", "b", "c", "d"]);
assert(vygraph.shortest_path(g, "a", "d") == ["a", "b", "c", "d"]);
assert(vygraph.dijkstra(g, "a")["dist"]["d"] == 6.0);

// 7. vysemver
assert(vysemver.compare("1.0.0-alpha.1", "1.0.0-alpha.beta") < 0);
assert(vysemver.compare("1.0.0-rc.1", "1.0.0") < 0);
assert(vysemver.satisfies("1.4.2", "^1.2.0"));
assert(not vysemver.satisfies("2.0.0", "^1.2.0"));
assert(vysemver.max_satisfying(["1.0.0", "1.4.2", "2.0.0"], ">=1.0.0 <2.0.0") == "1.4.2");
assert(vysemver.bump_minor("1.4.2") == "1.5.0");
"#,
        )
        .unwrap();

        let loaded = load_source(main_vy.to_string_lossy().as_ref()).unwrap();
        assert_eq!(run_source(&loaded), Ok(()));
        drop(_guard);

        let _ = fs::remove_dir_all(proj);
    }

    #[test]
    fn pillar5_scoped_packages_workspaces_platform_native_and_py_bridge() {
        use crate::registry::{
            current_platform_key, parse_pkg_spec, pkg_add, pkg_audit, pkg_index, pkg_install,
            pkg_py_bridge, pkg_remove, select_native_spec,
        };
        use sha2::{Digest, Sha256};

        // 1. Scoped semver spec parsing
        assert_eq!(
            parse_pkg_spec("@acme/core@^1.2.0").unwrap(),
            ("@acme/core", Some("^1.2.0"))
        );
        assert_eq!(parse_pkg_spec("@acme/core").unwrap(), ("@acme/core", None));
        assert!(parse_pkg_spec("@acme/").is_err());

        // 2. Per-platform native spec selector
        let plat = current_platform_key();
        let nat_map = serde_json::json!({
            plat.clone(): { "library": "custom_plat.dll", "source": "src/plat.rs" },
            "other-arch": { "library": "other.so" }
        });
        let picked = select_native_spec(&nat_map).unwrap();
        assert_eq!(
            picked.get("library").and_then(|v| v.as_str()),
            Some("custom_plat.dll")
        );

        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let work = env::temp_dir().join(format!("veyra_pillar5_local_{stamp}"));
        let reg_good = work.join("reg_good");
        let reg_pkg_src = work.join("src_scoped_utils");
        let monorepo = work.join("monorepo");
        let ws_core = monorepo.join("packages").join("core");
        let ws_app = monorepo.join("apps").join("web");
        let home_dir = work.join("veyra_home");

        fs::create_dir_all(&reg_good).unwrap();
        fs::create_dir_all(&reg_pkg_src).unwrap();
        fs::create_dir_all(ws_core.join("src")).unwrap();
        fs::create_dir_all(ws_app.join("src")).unwrap();
        fs::create_dir_all(&home_dir).unwrap();

        // Scoped package stored in secondary federated registry with per-platform native artifact
        fs::write(
            reg_pkg_src.join("veyra.json"),
            r#"{"name":"@acme/utils","version":"1.2.3","entry":"utils.vy","packages":{}}"#,
        )
        .unwrap();
        fs::write(
            reg_pkg_src.join("utils.vy"),
            "fn triple(x: Int) -> Int {\n    return x * 3;\n}\n",
        )
        .unwrap();
        let fake_native_bytes = b"veyra-platform-native-stub";
        fs::write(reg_good.join("native_stub.bin"), fake_native_bytes).unwrap();
        let mut nat_hash = Sha256::new();
        nat_hash.update(fake_native_bytes);
        let nat_sha = format!("{:x}", nat_hash.finalize());
        let reg_index = serde_json::json!({
            "registry_version": 1,
            "packages": {
                "@acme/utils": {
                    "version": "1.2.3",
                    "entry": "utils.vy",
                    "source": "../src_scoped_utils",
                    "native": {
                        plat: {
                            "library": "native_stub.bin",
                            "url": "native_stub.bin",
                            "sha256": nat_sha
                        }
                    }
                }
            }
        });
        fs::write(
            reg_good.join("index.json"),
            serde_json::to_string_pretty(&reg_index).unwrap(),
        )
        .unwrap();

        // Monorepo workspace root + core package + app consuming both workspace & @acme/utils
        fs::write(
            monorepo.join("veyra.json"),
            serde_json::json!({
                "name": "acme-monorepo",
                "version": "0.1.0",
                "workspaces": ["packages/*", "apps/*"],
                "packages": {}
            })
            .to_string(),
        )
        .unwrap();
        fs::write(
            ws_core.join("veyra.json"),
            r#"{"name":"@acme/core","version":"2.0.0","entry":"src/lib.vy","packages":{}}"#,
        )
        .unwrap();
        fs::write(
            ws_core.join("src").join("lib.vy"),
            "fn base_val() -> Int {\n    return 14;\n}\n",
        )
        .unwrap();
        fs::write(
            ws_app.join("veyra.json"),
            serde_json::json!({
                "name": "web-app",
                "version": "0.1.0",
                "entry": "src/main.vy",
                "packages": {
                    "@acme/core": "^2.0.0"
                }
            })
            .to_string(),
        )
        .unwrap();
        let app_main = ws_app.join("src").join("main.vy");
        fs::write(
            &app_main,
            r#"use "@acme/utils";
use "@acme/core";
let u = import_package("@acme/utils");
let c = import_package("@acme/core");
assert(u.triple(c.base_val()) == 42);
"#,
        )
        .unwrap();

        let bad_reg = work.join("does_not_exist").join("index.json");
        let reg_urls = format!("{};{}", bad_reg.display(), reg_good.display());
        let home_str = home_dir.to_string_lossy().into_owned();

        let _guard = EnvCwdGuard::enter(
            &ws_app,
            &[
                ("VEYRA_REGISTRY_URLS", Some(reg_urls.as_str())),
                ("VEYRA_REGISTRY_URL", None),
                ("VEYRA_HOME", Some(home_str.as_str())),
                ("VEYRA_OFFLINE", None),
            ],
        );

        // Federated index fails over from missing first registry to second registry
        let (_, idx) = pkg_index().unwrap();
        assert!(
            idx.get("packages")
                .and_then(|p| p.get("@acme/utils"))
                .is_some()
        );

        // Add scoped package via federated registry + platform native artifact + lockfile audit
        pkg_add("@acme/utils@^1.0.0").unwrap();
        assert!(
            ws_app
                .join("vendor")
                .join("@acme")
                .join("utils")
                .join("native_stub.bin")
                .is_file()
        );

        // Install from monorepo root restores workspace member @acme/core into apps/web/vendor/@acme/core
        env::set_current_dir(&monorepo).unwrap();
        pkg_install().unwrap();
        assert!(
            ws_app
                .join("vendor")
                .join("@acme")
                .join("core")
                .join("src")
                .join("lib.vy")
                .is_file()
        );

        env::set_current_dir(&ws_app).unwrap();
        pkg_audit().unwrap();

        // Generate py-bridge wrapper for Python stdlib `math` inside ws_app
        pkg_py_bridge("math", None, false).unwrap();
        let py_math_dir = ws_app.join("vendor").join("py_math");
        assert!(py_math_dir.join("py_math.vy").is_file());
        fs::write(
            ws_app.join("src").join("bridge_check.vy"),
            r#"use "@acme/utils";
use "@acme/core";
use "py_math";
let u = import_package("@acme/utils");
let c = import_package("@acme/core");
let pym = import_package("py_math");
assert(u.triple(c.base_val()) == 42);
if py_available() {
    assert(pym.call("sqrt", [16]) == 4.0);
}
"#,
        )
        .unwrap();

        let src = load_source(app_main.to_string_lossy().as_ref()).unwrap();
        assert_eq!(run_source(&src), Ok(()));
        let bridge_src = load_source(
            ws_app
                .join("src")
                .join("bridge_check.vy")
                .to_string_lossy()
                .as_ref(),
        )
        .unwrap();
        assert_eq!(run_source(&bridge_src), Ok(()));

        // Scoped package removal cleans up vendor/@acme/@... cleanly
        pkg_remove("@acme/utils").unwrap();
        assert!(!ws_app.join("vendor").join("@acme").join("utils").exists());
        assert!(ws_app.join("vendor").join("@acme").join("core").is_dir());

        drop(_guard);
        let _ = fs::remove_dir_all(work);
    }

    #[cfg(feature = "native-services")]
    #[test]
    fn pillar5_http_federated_registry_failover_and_offline_cache() {
        use crate::registry::{pkg_add, pkg_index};
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let work = env::temp_dir().join(format!("veyra_pillar5_http_{stamp}"));
        let srv_home = work.join("srv_home");
        let client_home = work.join("client_home");
        let app_a = work.join("app_a");
        let app_b = work.join("app_b");

        fs::create_dir_all(srv_home.join("registry")).unwrap();
        fs::create_dir_all(srv_home.join("packages").join("fedlib")).unwrap();
        fs::create_dir_all(&client_home).unwrap();
        fs::create_dir_all(app_a.join("src")).unwrap();
        fs::create_dir_all(app_b.join("src")).unwrap();

        fs::write(
            srv_home.join("packages").join("fedlib").join("veyra.json"),
            r#"{"name":"fedlib","version":"1.0.0","entry":"lib.vy","packages":{}}"#,
        )
        .unwrap();
        fs::write(
            srv_home.join("packages").join("fedlib").join("lib.vy"),
            "fn ping() -> Int {\n    return 99;\n}\n",
        )
        .unwrap();
        let bundle =
            crate::registry::build_package_bundle(&srv_home.join("packages").join("fedlib"))
                .unwrap();
        fs::create_dir_all(srv_home.join("bundles")).unwrap();
        fs::write(
            srv_home.join("bundles").join("fedlib.vypkg.json"),
            serde_json::to_string_pretty(&bundle).unwrap(),
        )
        .unwrap();
        fs::write(
            srv_home.join("registry").join("index.json"),
            serde_json::json!({
                "registry_version": 1,
                "packages": {
                    "fedlib": {
                        "version": "1.0.0",
                        "entry": "lib.vy",
                        "bundle_path": "/bundles/fedlib.vypkg.json"
                    }
                }
            })
            .to_string(),
        )
        .unwrap();

        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let srv_url = format!("http://127.0.0.1:{port}");
        let srv_home_thread = srv_home.clone();
        std::thread::spawn(move || {
            let _ = crate::server::serve_registry(&srv_home_thread, port);
        });
        let mut ready = false;
        for _ in 0..40 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                ready = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert!(
            ready,
            "federated registry server failed to listen on {srv_url}"
        );

        for app in [&app_a, &app_b] {
            fs::write(
                app.join("veyra.json"),
                r#"{"name":"app","version":"0.1.0","entry":"src/main.vy","packages":{}}"#,
            )
            .unwrap();
            fs::write(
                app.join("src").join("main.vy"),
                "use \"fedlib\";\nlet f = import_package(\"fedlib\");\nassert(f.ping() == 99);\n",
            )
            .unwrap();
        }

        // Allocate an unused closed port for primary failing registry
        let dead_probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let dead_port = dead_probe.local_addr().unwrap().port();
        drop(dead_probe);
        let fed_urls = format!("http://127.0.0.1:{dead_port},{srv_url}");
        let home_str = client_home.to_string_lossy().into_owned();

        let _guard = EnvCwdGuard::enter(
            &app_a,
            &[
                ("VEYRA_REGISTRY_URLS", Some(fed_urls.as_str())),
                ("VEYRA_REGISTRY_URL", None),
                ("VEYRA_HOME", Some(home_str.as_str())),
                ("VEYRA_OFFLINE", None),
            ],
        );

        let (_, idx) = pkg_index().unwrap();
        assert!(idx.get("packages").and_then(|p| p.get("fedlib")).is_some());
        pkg_add("fedlib").unwrap();
        assert!(app_a.join("vendor").join("fedlib").join("lib.vy").is_file());

        // Populate offline cache verified under VEYRA_HOME/cache
        assert!(client_home.join("cache").join("registries").is_dir());
        assert!(client_home.join("cache").join("artifacts").is_dir());

        // Offline mode installs into app_b purely from disk cache
        unsafe { env::set_var("VEYRA_OFFLINE", "1") };
        unsafe { env::set_var("VEYRA_REGISTRY_URLS", &srv_url) };
        env::set_current_dir(&app_b).unwrap();
        pkg_add("fedlib").unwrap();
        let src_b =
            load_source(app_b.join("src").join("main.vy").to_string_lossy().as_ref()).unwrap();
        assert_eq!(run_source(&src_b), Ok(()));

        drop(_guard);
        let _ = fs::remove_dir_all(work);
    }

    #[test]
    fn wasm_compiler_and_stack_vm_roundtrip() {
        let src = r#"
fn fib(n: Int) -> Int {
    if n <= 1 { return n; }
    return fib(n - 1) + fib(n - 2);
}
fn sum_skip(n: Int) -> Int {
    let s = 0;
    for i in range(0, n) {
        if i % 2 == 0 { continue; }
        s = s + i;
    }
    return s;
}
let out = fib(10) + sum_skip(6);
print(out);
return out;
"#;
        let bytes = crate::native_codegen::compile_source_to_wasm(src).unwrap();
        assert_eq!(&bytes[0..8], b"\0asm\x01\0\0\0");
        let wat = crate::native_codegen::wasm_disasm(&bytes).unwrap();
        assert!(wat.contains("(export \"fib\""));
        let (fib10, _) = crate::native_codegen::wasm_run(&bytes, "fib", &[10.0]).unwrap();
        assert_eq!(fib10, 55.0);
        let (main_res, prints) = crate::native_codegen::wasm_run(&bytes, "main", &[]).unwrap();
        assert_eq!(main_res, 64.0);
        assert_eq!(prints, vec!["64".to_string()]);

        // Also test via Veyra VM builtins
        let vy = r#"
let b = wasm_compile("fn sq(x) { return x * x; } return sq(9);");
let r = wasm_run(b, "main");
assert(r["result"] == 81);
let r2 = wasm_run(b, "sq", [12]);
assert(r2["result"] == 144);
"#;
        assert_eq!(run_source(vy), Ok(()));
    }

    #[test]
    fn direct_zero_toolchain_native_pe_and_elf() {
        let src = r#"
fn fib(n: Int) -> Int {
    if n <= 1 { return n; }
    return fib(n - 1) + fib(n - 2);
}
let acc = 0;
for i in range(0, 6) {
    if i == 3 { continue; }
    acc = acc + i;
}
let ans = fib(10) + acc;
print(ans);
return ans - 25;
"#;
        // Verify ELF64 emission on all platforms
        let elf = crate::native_codegen::compile_source_to_direct_binary(
            src,
            crate::native_codegen::DirectNativeFormat::Elf64,
        )
        .unwrap();
        assert_eq!(&elf[0..4], b"\x7FELF");
        assert_eq!(u16::from_le_bytes([elf[18], elf[19]]), 62); // EM_X86_64

        // Verify PE32+ emission and actually execute it live on Windows x86_64!
        let dir = env::temp_dir().join("veyra_direct_pe_test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let vy_path = dir.join("prog.vy");
        let exe_path = dir.join("prog.exe");
        fs::write(&vy_path, src).unwrap();
        let wasm_out = dir.join("prog.wasm");
        crate::native_codegen::compile_file_to_wasm(&vy_path, &wasm_out).unwrap();
        assert!(wasm_out.is_file());

        crate::native_codegen::compile_file_to_direct_native(
            &vy_path,
            Some(&exe_path),
            Some(crate::native_codegen::DirectNativeFormat::Pe64),
        )
        .unwrap();
        let pe_bytes = fs::read(&exe_path).unwrap();
        assert_eq!(&pe_bytes[0..2], b"MZ");

        #[cfg(all(windows, target_arch = "x86_64"))]
        {
            let output = std::process::Command::new(&exe_path).output().unwrap();
            assert_eq!(
                output.status.code(),
                Some(42),
                "unexpected exit code, stderr: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert_eq!(stdout.trim(), "67");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn contracts_and_static_pure_effect_checker() {
        // 1. Valid @pure, @effect, requires, ensures work cleanly in VM
        let ok_src = r#"
@pure
fn safe_div(a: Int, b: Int) -> Int {
    requires(b != 0, "divisor must be non-zero");
    let res = int(a / b);
    ensures(res * b <= a);
    return res;
}
@effect("io")
fn log_div(a: Int, b: Int) {
    let x = safe_div(a, b);
    print(x);
}
assert(safe_div(20, 4) == 5);
log_div(21, 5);
"#;
        assert_eq!(run_source(ok_src), Ok(()));

        // 2. Runtime contract violations report ContractError cleanly
        let runtime_fail = run_source("fn f(x) { requires(x > 0, \"pos\"); return x; } f(-1);");
        assert!(runtime_fail.is_err());
        assert!(runtime_fail.unwrap_err().contains("ContractError: pos"));

        let post_fail =
            run_source("fn f(x) { let r = x; ensures(r > 10, \"big\"); return r; } f(2);");
        assert!(post_fail.is_err());
        assert!(post_fail.unwrap_err().contains("ContractError: big"));

        // 3. Static literal contradiction caught BEFORE execution
        let static_false = compile_source("fn f() { requires(1 > 5); }");
        assert!(static_false.is_err());
        assert!(static_false.unwrap_err().contains("contract error"));

        // 4. Transitive @pure violation (direct or indirect I/O/fs/global mutation) caught statically
        let impure_io = compile_source("@pure fn bad(x) { print(x); return x; }");
        assert!(impure_io.is_err());
        assert!(impure_io.unwrap_err().contains("effect error"));

        let impure_transitive = compile_source(
            "fn helper() { write_file(\"x.txt\", \"y\"); }\n@pure fn top() { helper(); }",
        );
        assert!(impure_transitive.is_err());
        assert!(impure_transitive.unwrap_err().contains("effect error"));

        let impure_state = compile_source("let g = 1;\n@pure fn mut_g() { g = 2; }");
        assert!(impure_state.is_err());
        assert!(impure_state.unwrap_err().contains("effect error"));

        // 5. Undeclared capability in @effect rejected statically
        let missing_net = compile_source(
            "@effect(\"io\") fn fetch() { return http_get(\"http://example.com\"); }",
        );
        assert!(missing_net.is_err());
        assert!(missing_net.unwrap_err().contains("effect error"));

        // 6. AOT parity for @pure and contracts
        let dir = env::temp_dir().join("veyra_contracts_aot_test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let exe = dir.join(if cfg!(target_os = "windows") {
            "c.exe"
        } else {
            "c"
        });
        let aot_src = r#"
@pure
fn checked(n: Int) -> Int {
    requires(n >= 0, "nonneg");
    let out = n * 2;
    ensures(out >= 0, "out_nonneg");
    return out;
}
assert(checked(7) == 14);
"#;
        crate::aot::compile_source_to_native(aot_src, &exe).unwrap();
        let status = std::process::Command::new(&exe).status().unwrap();
        assert!(status.success());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn capability_sandbox_and_reproducible_pkg_bundle() {
        let dir = env::temp_dir().join("veyra_cap_repro_test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("src")).unwrap();
        let data_file = dir.join("note.txt");
        fs::write(&data_file, "hello-cap").unwrap();
        let escaped_file = data_file.to_string_lossy().replace('\\', "\\\\");

        // Attenuated capabilities block fs.write & process while permitting fs.read, and cannot escalate!
        let vy_src = format!(
            r#"
assert(cap_current() == "all");
let ok = cap_restrict(["fs.read"], fn() {{
    let txt = read_file("{escaped_file}");
    assert(txt == "hello-cap");
    let nested_blocked = false;
    try {{
        cap_restrict(["fs.write"], fn() {{
            write_file("{escaped_file}", "pwned");
        }});
    }} catch e {{
        nested_blocked = contains(str(e), "CapabilityError");
    }}
    assert(nested_blocked);
    return true;
}});
assert(ok == true);
assert(cap_current() == "all");
"#
        );
        assert_eq!(run_source(&vy_src), Ok(()));

        // Verify reproducible deterministic .vybc and .vypkg.json SHA-256 across rebuilds
        fs::write(
            dir.join("veyra.json"),
            r#"{"name":"capdemo","version":"1.2.3","entry":"src/main.vy","capabilities":["fs.write","fs.read"]}"#,
        )
        .unwrap();
        fs::write(dir.join("src").join("main.vy"), "let x = 21 * 2;").unwrap();

        let b1 = crate::registry::build_package_bundle(&dir).unwrap();
        let b2 = crate::registry::build_package_bundle(&dir).unwrap();
        assert_eq!(
            serde_json::to_string_pretty(&b1).unwrap(),
            serde_json::to_string_pretty(&b2).unwrap()
        );
        assert_eq!(
            b1["capabilities"],
            serde_json::json!(["fs.read", "fs.write"])
        );

        let bc1 = compile_to_vybc("let x = 21 * 2;").unwrap();
        let bc2 = compile_to_vybc("let x = 21 * 2;").unwrap();
        assert_eq!(bc1, bc2);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn gpu_fused_kernel_single_pass_spirv_and_hlsl() {
        let vy_src = r#"
let info_before = gpu_info();
let before_cnt = info_before["fused_kernels"];
let ops = ["add", "mul_scalar:2.0", "relu"];
let insp = gpu_fuse_inspect(ops);
let spv = insp["spirv_words"];
assert(len(spv) > 20);
assert(int(spv[0]) == 119734787);
assert(contains(insp["hlsl"], "CSMain"));

let y = gpu_fuse(ops, [-3.0, 1.0, 4.0], [1.0, 2.0, -5.0]);
assert(len(y) == 3);
assert(abs(y[0] - 0.0) < 0.0001);
assert(abs(y[1] - 6.0) < 0.0001);
assert(abs(y[2] - 0.0) < 0.0001);

let ta = tensor([1.0, 4.0], false);
let tb = tensor([3.0, 4.0], false);
let ty = gpu_fuse(["sub", "abs", "sigmoid"], ta, tb);
assert(tensor_shape(ty) == [2]);
let flat = tensor_values(ty);
assert(abs(flat[0] - (1.0 / (1.0 + exp(-2.0)))) < 0.0001);
assert(abs(flat[1] - 0.5) < 0.0001);

let info_after = gpu_info();
assert(info_after["fused_kernels"] >= before_cnt + 2);
"#;
        assert_eq!(run_source(vy_src), Ok(()));
    }

    #[test]
    fn py_bridge_dlpack_torch_and_numpy_interop() {
        if crate::py_bridge::py_eval("1 + 1", None).is_err() {
            return;
        }
        let vy_src = r#"
let has_np = py_eval("__import__('importlib.util', fromlist=['find_spec']).find_spec('numpy') is not None");
let has_torch = py_eval("__import__('importlib.util', fromlist=['find_spec']).find_spec('torch') is not None");
let t = tensor([[1.5, 2.5], [3.5, 4.5]], false);
let dl = tensor_to_dlpack(t);

if has_np {
    let np = py_import("numpy");
    let arr = np.from_dlpack(dl);
    assert(list(arr.shape) == [2, 2], "np shape: " + str(arr.shape));
    let s = np.sum(arr);
    assert(abs(float(s) - 12.0) < 0.0001, "np sum: " + str(s));
    let back = py_from_dlpack(arr);
    assert(tensor_shape(back) == [2, 2], "np back shape: " + str(tensor_shape(back)));
    let flat = tensor_values(back);
    assert(abs(flat[0] - 1.5) < 0.0001 and abs(flat[3] - 4.5) < 0.0001, "np flat: " + str(flat));
    py_free(arr);
    py_free(np);
}
py_free(dl);

if has_torch {
    let torch = py_import("torch");
    let py_builtins = py_import("builtins");
    let dl2 = tensor_to_dlpack(t);
    let pt = torch.from_dlpack(dl2);
    assert(py_builtins.list(pt.shape) == [2, 2], "torch shape: " + str(py_builtins.list(pt.shape)));
    let pt_sum = float(pt.sum().item());
    assert(abs(pt_sum - 12.0) < 0.0001, "torch sum: " + str(pt_sum));
    let back_t = py_from_dlpack(pt);
    assert(tensor_shape(back_t) == [2, 2], "torch back shape: " + str(tensor_shape(back_t)));
    let via_fast = py_to_tensor(pt);
    assert(tensor_shape(via_fast) == [2, 2], "torch fast shape: " + str(tensor_shape(via_fast)));
    let ft = tensor_values(via_fast);
    assert(abs(ft[1] - 2.5) < 0.0001 and abs(ft[2] - 3.5) < 0.0001, "torch flat: " + str(ft));
    py_free(pt);
    py_free(dl2);
    py_free(py_builtins);
    py_free(torch);
}
"#;
        assert_eq!(run_source(vy_src), Ok(()));
    }

    #[test]
    fn vm_mark_sweep_cycle_collector() {
        let vy_src = r#"
class Node {
    fn init(self, label: String) -> Null {
        self.label = label;
        self.next = null;
        return null;
    }
}

assert(gc_stats()["paused"] == false);
assert(gc_pause() == true);
assert(gc_stats()["paused"] == true);
assert(gc_resume() == true);
assert(gc_stats()["paused"] == false);

// Live cyclic root must survive collection.
let live_a = Node("liveA");
let live_b = Node("liveB");
live_a.next = live_b;
live_b.next = live_a;

fn build_garbage_cycles() -> Null {
    let i = 0;
    while i < 12 {
        let x = Node("dead" + str(i));
        let y = Node("deadPair" + str(i));
        x.next = y;
        y.next = x;
        i = i + 1;
    }
    return null;
}

build_garbage_cycles();
let rep1 = gc_collect();
assert(rep1["collected"] >= 24, "expected dead cycles swept, got " + str(rep1));
assert(live_a.label == "liveA" and live_a.next.label == "liveB" and live_a.next.next.label == "liveA");

// Dropping one cycle reclaims those 2 cyclic nodes too.
let orphan_x = Node("ox");
let orphan_y = Node("oy");
orphan_x.next = orphan_y;
orphan_y.next = orphan_x;
orphan_x = null;
orphan_y = null;
let rep2 = gc_collect();
assert(rep2["collected"] >= 2, "expected dropped orphan cycle swept, got " + str(rep2));
assert(gc_stats()["collections"] >= 2);
"#;
        assert_eq!(run_source(vy_src), Ok(()));
    }

    #[test]
    fn deterministic_simulation_replay_and_faults() {
        let vy_src = r#"
fn run_trace(seed: Int) -> List {
    sim_start(seed);
    assert(sim_now() == 0);
    sim_advance(25);
    let r1 = sim_rand();
    sim_fault("net.send", 1.0);
    sim_fault("net.rare", 0.0);
    assert(sim_fault("net.send") == true);
    assert(sim_fault("net.rare") == false);
    sim_advance(15);
    let r2 = sim_rand();
    sim_log({"step": "done", "t": sim_now()});
    let log = sim_log();
    sim_stop();
    return [r1, r2, log];
}

let t1 = run_trace(2026);
let t2 = run_trace(2026);
let t3 = run_trace(9999);
assert(t1[0] == t2[0] and t1[1] == t2[1]);
assert(t1[2] == t2[2]);
assert(t1[0] != t3[0]);
"#;
        assert_eq!(run_source(vy_src), Ok(()));
    }

    #[test]
    fn supervised_actors_mailbox_state_and_restart() {
        let vy_src = r#"
let counter = actor_spawn(fn(msg, state) {
    if msg["op"] == "add" {
        let next = state + msg["n"];
        return {"state": next, "reply": next};
    }
    if msg["op"] == "crash" {
        throw "transient failure";
    }
    if msg["op"] == "get" {
        return {"state": state, "reply": state};
    }
    return {"state": state, "reply": null};
}, {"init": 100, "max_restarts": 1});

assert(actor_ask(counter, {"op": "add", "n": 25}) == 125);
assert(actor_send(counter, {"op": "add", "n": 5}) == true);
assert(actor_ask(counter, {"op": "get"}) == 130);

// First failure triggers supervision restart back to init=100 while staying alive.
let err1 = "";
try {
    actor_ask(counter, {"op": "crash"});
} catch e {
    err1 = str(e);
}
assert(contains(err1, "restarted 1/1"), "unexpected err1: " + err1);
let st1 = actor_status(counter);
assert(st1["alive"] == true and st1["restarts"] == 1 and st1["status"] == "restarted");
assert(actor_ask(counter, {"op": "get"}) == 100);

// Second failure exceeds max_restarts=1 -> actor transitions to failed and terminates.
let err2 = "";
try {
    actor_ask(counter, {"op": "crash"});
} catch e {
    err2 = str(e);
}
assert(contains(err2, "exceeded max_restarts=1"), "unexpected err2: " + err2);
assert(actor_stop(counter) == true);
let st2 = actor_status(counter);
assert(st2["alive"] == false and st2["status"] == "failed");
"#;
        assert_eq!(run_source(vy_src), Ok(()));
    }

    #[test]
    fn ffi_header_bindgen_and_live_cdylib_call() {
        if std::process::Command::new("rustc")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let dir = env::temp_dir().join("veyra_ffi_bindgen_test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let rs_path = dir.join("cmath_abi.rs");
        fs::write(
            &rs_path,
            r#"
#[unsafe(no_mangle)]
pub extern "C" fn c_add_i32(a: i32, b: i32) -> i32 {
    a + b
}

#[unsafe(no_mangle)]
pub extern "C" fn c_fma_f64(x: f64, y: f64, z: f64) -> f64 {
    x * y + z
}
"#,
        )
        .unwrap();
        let lib_path = dir.join(format!(
            "cmath_abi.{}",
            crate::plugin::platform_shared_ext()
        ));
        crate::plugin::build_rust_plugin(&rs_path, &lib_path).unwrap();

        let header = r#"
// Sample C API header
typedef int i32_alias;
i32_alias c_add_i32(i32_alias a, i32_alias b);
double c_fma_f64(double x, double y, double z);
"#;
        let wrapper = crate::plugin::ffi_bindgen(header, &lib_path.to_string_lossy()).unwrap();
        let runner = format!(
            "{wrapper}\nassert(c_add_i32(19, 23) == 42);\nlet f = c_fma_f64(2.5, 4.0, 1.5);\nassert(abs(f - 11.5) < 0.0001);\n"
        );
        assert_eq!(run_source(&runner), Ok(()));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn dap_time_travel_step_back_and_reverse_continue() {
        use std::collections::VecDeque;
        use std::io::{BufReader, Read, Write};
        use std::sync::{Arc, Condvar, Mutex};

        #[derive(Clone, Default)]
        struct MemPipe {
            inner: Arc<(Mutex<(VecDeque<u8>, bool)>, Condvar)>,
        }
        impl MemPipe {
            fn close(&self) {
                let (m, cv) = &*self.inner;
                let mut g = m.lock().unwrap();
                g.1 = true;
                cv.notify_all();
            }
        }
        impl Read for MemPipe {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let (m, cv) = &*self.inner;
                let mut g = m.lock().unwrap();
                while g.0.is_empty() && !g.1 {
                    g = cv.wait(g).unwrap();
                }
                let mut n = 0;
                while n < buf.len() {
                    if let Some(b) = g.0.pop_front() {
                        buf[n] = b;
                        n += 1;
                    } else {
                        break;
                    }
                }
                Ok(n)
            }
        }
        impl Write for MemPipe {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                let (m, cv) = &*self.inner;
                let mut g = m.lock().unwrap();
                g.0.extend(buf);
                cv.notify_all();
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let prog = env::temp_dir().join("veyra_dap_timetravel_test.vy");
        fs::write(
            &prog,
            "let x = 1;\nx = 2;\nx = 3;\nx = 4;\nassert(x == 4);\n",
        )
        .unwrap();

        let mut c2s = MemPipe::default();
        let s2c = MemPipe::default();
        let server_in = c2s.clone();
        let server_out = s2c.clone();
        let srv_handle = std::thread::spawn(move || {
            crate::dap::run_dap_with_io(BufReader::new(server_in), Box::new(server_out))
        });

        let mut client_reader = BufReader::new(s2c);
        let mut seq = 1u64;
        let mut send_req = |cmd: &str, args: serde_json::Value| -> u64 {
            let id = seq;
            seq += 1;
            let payload = serde_json::json!({
                "seq": id,
                "type": "request",
                "command": cmd,
                "arguments": args
            });
            let body = serde_json::to_string(&payload).unwrap();
            let msg = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
            c2s.write_all(msg.as_bytes()).unwrap();
            c2s.flush().unwrap();
            id
        };
        let mut events: Vec<serde_json::Value> = Vec::new();
        fn recv_resp(
            reader: &mut impl std::io::BufRead,
            events: &mut Vec<serde_json::Value>,
            target_cmd: &str,
        ) -> serde_json::Value {
            loop {
                let msg = crate::dap::read_dap_message(reader)
                    .unwrap()
                    .expect("expected DAP message");
                if msg.get("type").and_then(|v| v.as_str()) == Some("response")
                    && msg.get("command").and_then(|v| v.as_str()) == Some(target_cmd)
                {
                    assert_eq!(
                        msg.get("success").and_then(|v| v.as_bool()),
                        Some(true),
                        "DAP command {target_cmd} failed: {msg}"
                    );
                    return msg;
                }
                events.push(msg);
            }
        }
        fn wait_event(
            reader: &mut impl std::io::BufRead,
            events: &mut Vec<serde_json::Value>,
            target_event: &str,
        ) -> serde_json::Value {
            if let Some(pos) = events
                .iter()
                .position(|m| m.get("event").and_then(|v| v.as_str()) == Some(target_event))
            {
                return events.remove(pos);
            }
            loop {
                let msg = crate::dap::read_dap_message(reader)
                    .unwrap()
                    .expect("expected DAP event");
                if msg.get("type").and_then(|v| v.as_str()) == Some("event")
                    && msg.get("event").and_then(|v| v.as_str()) == Some(target_event)
                {
                    return msg;
                }
                events.push(msg);
            }
        }

        send_req("initialize", serde_json::json!({"adapterID": "veyra"}));
        let init_resp = recv_resp(&mut client_reader, &mut events, "initialize");
        assert_eq!(init_resp["body"]["supportsStepBack"].as_bool(), Some(true));
        let _ = wait_event(&mut client_reader, &mut events, "initialized");

        send_req(
            "launch",
            serde_json::json!({"program": prog.to_string_lossy(), "stopOnEntry": false}),
        );
        let _ = recv_resp(&mut client_reader, &mut events, "launch");

        // Breakpoint on line 2 (x = 2;) and line 4 (x = 4;)
        send_req(
            "setBreakpoints",
            serde_json::json!({
                "source": {"path": prog.to_string_lossy()},
                "breakpoints": [{"line": 2}, {"line": 4}]
            }),
        );
        let _ = recv_resp(&mut client_reader, &mut events, "setBreakpoints");
        send_req("configurationDone", serde_json::json!({}));
        let _ = recv_resp(&mut client_reader, &mut events, "configurationDone");

        // Hits line 2 first (x == 1 before x = 2) -> continue -> hits line 4 (x == 3 before x = 4).
        let _ = wait_event(&mut client_reader, &mut events, "stopped");
        send_req("continue", serde_json::json!({"threadId": 1}));
        let _ = recv_resp(&mut client_reader, &mut events, "continue");
        let _ = wait_event(&mut client_reader, &mut events, "stopped");

        // Evaluate live line 4: x is 3
        send_req("evaluate", serde_json::json!({"expression": "x"}));
        let eval_live = recv_resp(&mut client_reader, &mut events, "evaluate");
        assert_eq!(eval_live["body"]["result"].as_str(), Some("3"));

        // stepBack one statement -> line 3 (x is 2)!
        send_req("stepBack", serde_json::json!({"threadId": 1}));
        let _ = recv_resp(&mut client_reader, &mut events, "stepBack");
        let _ = wait_event(&mut client_reader, &mut events, "stopped");
        send_req("evaluate", serde_json::json!({"expression": "x"}));
        let eval_back = recv_resp(&mut client_reader, &mut events, "evaluate");
        assert_eq!(eval_back["body"]["result"].as_str(), Some("2"));

        // reverseContinue -> stops back at line 2 breakpoint (x == 1)!
        send_req("reverseContinue", serde_json::json!({"threadId": 1}));
        let _ = recv_resp(&mut client_reader, &mut events, "reverseContinue");
        let _ = wait_event(&mut client_reader, &mut events, "stopped");
        send_req("evaluate", serde_json::json!({"expression": "x"}));
        let eval_rev = recv_resp(&mut client_reader, &mut events, "evaluate");
        assert_eq!(eval_rev["body"]["result"].as_str(), Some("1"));

        send_req("continue", serde_json::json!({"threadId": 1}));
        let _ = recv_resp(&mut client_reader, &mut events, "continue");
        let _ = wait_event(&mut client_reader, &mut events, "terminated");
        send_req("disconnect", serde_json::json!({}));
        let _ = recv_resp(&mut client_reader, &mut events, "disconnect");
        c2s.close();
        srv_handle.join().unwrap().unwrap();
        let _ = fs::remove_file(&prog);
    }

    #[test]
    fn notebook_reactive_stateful_session() {
        let dir = env::temp_dir().join("veyra_notebook_test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let nb_file = dir.join(".veyra-notebook.json");
        let mut nb = crate::server::NotebookSession::new(Some(nb_file.clone()));

        let r1 = nb.eval_cell(
            "c1",
            "let x = 10;\nfn dbl(v) { return v * 2; }\nprint(dbl(x));",
        );
        assert_eq!(r1.error, None);
        assert_eq!(r1.stdout.trim(), "20");

        let r2 = nb.eval_cell("c2", "x = dbl(x) + 5;\nprint(x);");
        assert_eq!(r2.error, None);
        assert_eq!(r2.stdout.trim(), "25");

        let r3 = nb.eval_cell("c3", "x * 2");
        assert_eq!(r3.error, None);
        assert_eq!(r3.value.as_deref(), Some("50"));

        assert!(nb_file.is_file());
        nb.reset();
        let r4 = nb.eval_cell("c4", "x");
        assert!(r4.error.is_some());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn jit_setindex_and_return_list_native_execution() {
        let vy_src = r#"
fn scale_list(xs: List, factor: Float) -> List {
    let i = 0;
    while i < len(xs) {
        xs[i] = xs[i] * factor + 1.0;
        i = i + 1;
    }
    return xs;
}

let before = jit_info();
let k = 0;
let last = [];
while k < 20 {
    last = scale_list([1.0, 2.0, 3.0], 2.0);
    k = k + 1;
}
assert(last == [3, 5, 7], "unexpected jit list result: " + str(last));
let after = jit_info();
if jit_available() {
    assert(after["compiled_functions"] >= before["compiled_functions"]);
    assert(after["native_calls"] > before["native_calls"]);
}
"#;
        assert_eq!(run_source(vy_src), Ok(()));
    }
}
