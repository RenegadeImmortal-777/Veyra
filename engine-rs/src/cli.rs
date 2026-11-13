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
    println!("engine: native Rust bytecode VM");
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
        "Veyra {VERSION}\n\nUsage:\n  veyra run [file.vy]\n  veyra check [file.vy]\n  veyra lint [file.vy]\n  veyra debug [file.vy]\n  veyra dap\n  veyra ast [file.vy]\n  veyra symbols [file.vy]\n  veyra lsp\n  veyra test\n  veyra bench [file.vy] [iterations]\n  veyra profile [file.vy]\n  veyra doc [file.vy]\n  veyra aot [file.vy] [out_exe]\n  veyra build [--native]\n  veyra audit\n  veyra serve [dir] [port]\n  veyra web <routes.json> [port]\n  veyra app <app.vy> <routes.json> [port]\n  veyra disasm [file.vy]\n  veyra fmt <file.vy>\n  veyra new <name>\n  veyra pkg list\n  veyra pkg add <name>\n  veyra pkg remove <name>\n  veyra pkg installed\n  veyra doctor\n  veyra repl\n  veyra --version"
    );
}
