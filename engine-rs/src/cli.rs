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
        .unwrap_or("veyra-app")
        .to_string();
    fs::create_dir_all(p.join("src")).map_err(|e| e.to_string())?;
    fs::create_dir_all(p.join("tests")).map_err(|e| e.to_string())?;
    fs::create_dir_all(p.join("vendor")).map_err(|e| e.to_string())?;
    let manifest = serde_json::json!({
        "name": project_name,
        "version": "0.1.0",
        "entry": "src/main.vy",
        "edition": "2026",
        "engine": VERSION,
        "packages": {}
    });
    fs::write(
        p.join("veyra.json"),
        serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    fs::write(
        p.join("src/main.vy"),
        "const language: String = \"Veyra\";\nprint(\"Hello from\", language);\n",
    )
    .map_err(|e| e.to_string())?;
    println!("Created Veyra project {name}");
    Ok(())
}

pub(crate) fn doctor() {
    println!("Veyra {VERSION}");
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
        "plugin-abi: veyra_plugin_v{}",
        crate::plugin::VEYRA_PLUGIN_ABI_VERSION
    );
    println!("target: {}", env::consts::ARCH);
    println!("os: {}", env::consts::OS);
    println!("status: OK");
}

pub(crate) fn repl() -> Result<(), String> {
    println!("Veyra {VERSION} native REPL — :quit to exit");
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
        "Veyra {VERSION}\n\nUsage:\n  veyra run [--caps c1,c2] [file.vy]\n  veyra check [file.vy]\n  veyra verify [file.vy]\n  veyra lint [file.vy]\n  veyra debug [file.vy]\n  veyra dap\n  veyra ast [file.vy]\n  veyra symbols [file.vy]\n  veyra lsp\n  veyra test\n  veyra bench [file.vy] [iterations]\n  veyra profile [file.vy]\n  veyra doc [file.vy]\n  veyra aot [--direct] [--elf|--pe] [file.vy] [out_exe]\n  veyra wasm [--wasi] [file.vy] [-o out.wasm] [--disasm] [--run <fn> [args...]]\n  veyra ffi bindgen <header.h> <lib_path> [-o out.vy]\n  veyra notebook [dir] [port]\n  veyra build [--native]\n  veyra audit\n  veyra serve [dir] [port]\n  veyra web <routes.json> [port]\n  veyra app <app.vy> <routes.json> [port]\n  veyra disasm [file.vy]\n  veyra fmt <file.vy>\n  veyra new <name>\n  veyra pkg list\n  veyra pkg search <term>\n  veyra pkg info <name>\n  veyra pkg tree\n  veyra pkg add <name[@semver]>\n  veyra pkg remove <name>\n  veyra pkg update [name]\n  veyra pkg installed\n  veyra pkg install\n  veyra pkg publish\n  veyra pkg keygen [name]\n  veyra pkg sign [dir] [key]\n  veyra pkg verify [target]\n  veyra pkg pack [dir] [out.vypkg.json]\n  veyra pkg unpack <bundle.vypkg.json> [dest]\n  veyra pkg build-plugin [dir]\n  veyra pkg py-bridge <pypi_pkg> [out_dir] [--pip-install]\n  veyra pkg serve [dir] [port]\n  veyra doctor\n  veyra repl\n  veyra --version"
    );
}
