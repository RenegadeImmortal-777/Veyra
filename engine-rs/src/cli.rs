// CLI commands that are not part of the compile pipeline: project scaffolding,
// doctor, REPL and usage.
//
// Extracted from `main.rs` without behavior change; see docs/ENGINE-SPLIT.md.

use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

use crate::VERSION;
use crate::pipeline::run_source;

pub(crate) fn new_project(name: &str) -> Result<(), String> {
    let p = PathBuf::from(name);
    if p.exists() {
        return Err(format!("'{name}' already exists"));
    }
    let project_name = p
        .file_name()
        .and_then(|x| x.to_str())
        .filter(|x| !x.is_empty())
        .unwrap_or("kovrek-app")
        .to_string();
    fs::create_dir_all(p.join("src")).map_err(|e| e.to_string())?;
    fs::create_dir_all(p.join("tests")).map_err(|e| e.to_string())?;
    fs::create_dir_all(p.join("vendor")).map_err(|e| e.to_string())?;
    let manifest = serde_json::json!({
        "name": project_name,
        "version": "0.1.0",
        "entry": "src/main.kvr",
        "edition": "2026",
        "engine": VERSION,
        "packages": {}
    });
    fs::write(
        p.join("kovrek.json"),
        serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    fs::write(
        p.join("src/main.kvr"),
        "const language: String = \"Kovrek\";\nprint(\"Hello from\", language);\n",
    )
    .map_err(|e| e.to_string())?;
    println!("Created Kovrek project {name}");
    Ok(())
}

pub(crate) fn doctor() {
    println!("Kovrek {VERSION}");
    println!("engine: native Rust bytecode VM + AOT");
    println!(
        "jit: {}",
        if crate::jit::jit_available() {
            "x86_64-sse2 (active)"
        } else {
            "interpreter-only"
        }
    );
    let gpu = crate::gpu::gpu_info_val();
    if let crate::value::Val::Map(pairs) = gpu {
        let backend = crate::value::map_lookup(&pairs, &crate::value::Val::Str("backend".into()))
            .map(crate::value::Val::show)
            .unwrap_or_else(|| "cpu-fallback".into());
        let adapter =
            crate::value::map_lookup(&pairs, &crate::value::Val::Str("adapter_name".into()))
                .map(crate::value::Val::show)
                .unwrap_or_else(|| "CPU".into());
        println!("gpu: {backend} ({adapter})");
    }
    println!(
        "plugin-abi: kovrek_plugin_v{}",
        crate::plugin::KOVREK_PLUGIN_ABI_VERSION
    );
    println!("target: {}", env::consts::ARCH);
    println!("os: {}", env::consts::OS);
    println!("status: OK");
}

pub(crate) fn repl() -> Result<(), String> {
    println!("Kovrek {VERSION} native REPL — :quit to exit");
    let mut line = String::new();
    loop {
        print!("vy> ");
        io::stdout().flush().map_err(|e| e.to_string())?;
        line.clear();
        if io::stdin()
            .read_line(&mut line)
            .map_err(|e| e.to_string())?
            == 0
        {
            break;
        }
        if line.trim() == ":quit" {
            break;
        }
        if let Err(e) = run_source(&line) {
            eprintln!("error: {e}")
        }
    }
    Ok(())
}

pub(crate) fn usage() {
    println!(
        "Kovrek {VERSION}\n\nUsage:\n  kovrek run [--caps c1,c2] [file.kvr]\n  kovrek check [file.kvr]\n  kovrek verify [file.kvr]\n  kovrek lint [file.kvr]\n  kovrek debug [file.kvr]\n  kovrek dap\n  kovrek ast [file.kvr]\n  kovrek symbols [file.kvr]\n  kovrek lsp\n  kovrek test\n  kovrek bench [file.kvr] [iterations]\n  kovrek profile [file.kvr]\n  kovrek doc [file.kvr]\n  kovrek aot [--direct] [--elf|--pe] [file.kvr] [out_exe]\n  kovrek wasm [--wasi] [file.kvr] [-o out.wasm] [--disasm] [--run <fn> [args...]]\n  kovrek ffi bindgen <header.h> <lib_path> [-o out.kvr]\n  kovrek notebook [dir] [port]\n  kovrek build [--native]\n  kovrek audit\n  kovrek serve [dir] [port]\n  kovrek web <routes.json> [port]\n  kovrek app <app.kvr> <routes.json> [port]\n  kovrek disasm [file.kvr]\n  kovrek fmt <file.kvr>\n  kovrek new <name>\n  kovrek pkg list\n  kovrek pkg search <term>\n  kovrek pkg info <name>\n  kovrek pkg tree\n  kovrek pkg add <name[@semver]>\n  kovrek pkg remove <name>\n  kovrek pkg update [name]\n  kovrek pkg installed\n  kovrek pkg install\n  kovrek pkg publish\n  kovrek pkg keygen [name]\n  kovrek pkg sign [dir] [key]\n  kovrek pkg verify [target]\n  kovrek pkg pack [dir] [out.kvpkg.json]\n  kovrek pkg unpack <bundle.kvpkg.json> [dest]\n  kovrek pkg build-plugin [dir]\n  kovrek pkg py-bridge <pypi_pkg> [out_dir] [--pip-install]\n  kovrek pkg serve [dir] [port]\n  kovrek doctor\n  kovrek repl\n  kovrek --version"
    );
}
