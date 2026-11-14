// CLI commands that are not part of the compile pipeline: project scaffolding,
// doctor, REPL and usage.
//
// Extracted from `main.rs` without behavior change; see docs/ENGINE-SPLIT.md.

use std::env;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use crate::VERSION;
use crate::pipeline::{compile_source, load_source, run_kvbc, run_source, verify_source_strict};
use crate::server::NotebookSession;

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

/// Executes a `.kvr` source file or `.kvbc` bytecode file from disk.
pub(crate) fn exec_file_path(path: &Path) -> Result<(), String> {
    if path.extension().and_then(|e| e.to_str()) == Some("kvbc") {
        let bc = fs::read_to_string(path).map_err(|e| e.to_string())?;
        run_kvbc(&bc)
    } else {
        load_source(path.to_string_lossy().as_ref()).and_then(|s| run_source(&s))
    }
}

/// Launches a `.kvr` or `.kvbc` file (e.g. from Windows Explorer) and pauses
/// on exit when attached to an interactive terminal so output remains visible.
pub(crate) fn launch_file(path: &Path) -> Result<(), String> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("script");
    print!("\x1b]0;Kovrek - {name}\x07");
    let _ = io::stdout().flush();
    let res = exec_file_path(path);
    if io::stdin().is_terminal() {
        if let Err(ref e) = res {
            eprintln!("kovrek: {e}");
        }
        print!("\n[Kovrek finished — press Enter to close]");
        let _ = io::stdout().flush();
        let mut buf = String::new();
        let _ = io::stdin().read_line(&mut buf);
    }
    res
}

fn split_cli_args(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for ch in raw.chars() {
        match ch {
            '"' => in_quotes = !in_quotes,
            c if c.is_whitespace() && !in_quotes => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn unclosed_delimiters(src: &str) -> i32 {
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escaped = false;
    let mut chars = src.chars().peekable();
    while let Some(ch) = chars.next() {
        if in_str {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_str = false;
            }
            continue;
        }
        if ch == '"' {
            in_str = true;
        } else if ch == '/' && chars.peek() == Some(&'/') {
            for c in chars.by_ref() {
                if c == '\n' {
                    break;
                }
            }
        } else if matches!(ch, '(' | '[' | '{') {
            depth += 1;
        } else if matches!(ch, ')' | ']' | '}') {
            depth = (depth - 1).max(0);
        }
    }
    depth
}

fn print_terminal_help() {
    println!(
        "Kovrek Super-Terminal — Kovrek + PowerShell + Git Bash + CMD\n\
         \n\
         Shell & Navigation:\n\
         \x20 cd [dir | ~ | -]        Change folder      pwd                 Current folder\n\
         \x20 ls [-a] [path]          List files (color) cat <file>          View file with lines\n\
         \x20 mkdir / touch / rm      File ops           cp / mv / which     Copy, move, locate\n\
         \x20 clear / cls             Clear screen       env / export K=V    Environment vars\n\
         \n\
         Multi-Shell Superpowers:\n\
         \x20 ! <cmd>                 Run CMD command    ps <cmd>            Run PowerShell command\n\
         \x20 bash <cmd>              Run Git Bash cmd   <git/cargo/py...>   Auto-runs OS commands\n\
         \x20 let x = !cmd            Capture CMD/ps/bash stdout directly into Kovrek var x!\n\
         \n\
         Kovrek Tooling:\n\
         \x20 .run <file.kvr|.kvbc>   Execute file       .watch <file.kvr>   Auto-run on save\n\
         \x20 .check <file.kvr>       Type/compile check .verify <file.kvr>  Formal verification\n\
         \x20 .bundle [--gui] f [out] Build standalone .exe (--gui hides console on launch)\n\
         \x20 .vars                   Show Kovrek vars   .clear              Reset Kovrek state\n\
         \x20 .history                Command history    .exit               Quit terminal"
    );
}

fn handle_terminal_command(
    line: &str,
    session: &mut NotebookSession,
    term_state: &crate::term_ui::TerminalState,
) -> Result<bool, String> {
    let args = split_cli_args(line);
    let Some(cmd) = args.first().map(String::as_str) else {
        return Ok(true);
    };
    match cmd {
        ".exit" | ".quit" | ":quit" | ":exit" => return Ok(false),
        ".help" | ":help" => print_terminal_help(),
        ".version" => println!("Kovrek {VERSION}"),
        ".clear" => {
            session.reset();
            println!("Session cleared.");
        }
        ".history" => {
            for (i, h) in term_state.history.iter().enumerate() {
                println!("  {:>3}  {}", i + 1, h);
            }
        }
        ".vars" => {
            let vars = session.session_vars();
            if vars.is_empty() {
                println!("(no variables)");
            } else {
                for (name, val) in vars {
                    println!("{name} = {val}");
                }
            }
        }
        ".run" => {
            let p = args.get(1).ok_or("usage: .run <file.kvr|.kvbc>")?;
            exec_file_path(Path::new(p))?;
        }
        ".watch" => {
            let p = args.get(1).ok_or("usage: .watch <file.kvr>")?;
            crate::term_ui::watch_file(Path::new(p))?;
        }
        ".check" => {
            let p = args.get(1).ok_or("usage: .check <file.kvr>")?;
            let src = load_source(p)?;
            compile_source(&src)?;
            println!("OK");
        }
        ".verify" => {
            let p = args.get(1).ok_or("usage: .verify <file.kvr>")?;
            let src = load_source(p)?;
            let report = verify_source_strict(&src)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&report).unwrap_or_else(|_| report.to_string())
            );
        }
        ".bundle" => {
            let mut gui = false;
            let mut pos = Vec::new();
            for a in args.iter().skip(1) {
                if a == "--gui" {
                    gui = true;
                } else {
                    pos.push(a.as_str());
                }
            }
            let p = pos
                .first()
                .ok_or("usage: .bundle [--gui] <file.kvr> [out]")?;
            let out = pos.get(1).map(Path::new);
            let built = crate::bundle_executable_with_options(Path::new(p), out, gui)?;
            println!("Bundled standalone executable: {}", built.display());
        }
        _ => return Err(format!("unknown command '{cmd}' (try .help)")),
    }
    Ok(true)
}

pub(crate) fn repl() -> Result<(), String> {
    crate::term_ui::enable_virtual_terminal();
    print!("\x1b]0;Kovrek Super-Terminal\x07");
    let mut term_state = crate::term_ui::TerminalState::new();
    let mut session = NotebookSession::new(None);
    if io::stdin().is_terminal() {
        crate::term_ui::print_banner(&term_state);
    } else {
        println!("Kovrek {VERSION} Terminal — type Kovrek code or .help (.exit to quit)");
    }

    let mut buffer = String::new();
    loop {
        let (prompt, vis_len) =
            crate::term_ui::render_prompt(&term_state, &session, !buffer.is_empty());
        let Some(raw_line) =
            crate::term_ui::read_interactive_line(&prompt, vis_len, &term_state, &session)?
        else {
            if !buffer.trim().is_empty() {
                match session.eval_repl(&buffer) {
                    Ok(Some(v)) => println!("{v}"),
                    Ok(None) => {}
                    Err(e) => eprintln!("error: {e}"),
                }
            }
            break;
        };

        if buffer.is_empty() {
            let trimmed = raw_line.trim();
            if trimmed.is_empty() {
                continue;
            }
            term_state.push_history(trimmed);
            if trimmed.starts_with('.') || trimmed.starts_with(':') {
                match handle_terminal_command(trimmed, &mut session, &term_state) {
                    Ok(true) => continue,
                    Ok(false) => break,
                    Err(e) => {
                        eprintln!("error: {e}");
                        term_state.last_exit = 1;
                        continue;
                    }
                }
            }
            let split = split_cli_args(trimmed);
            match crate::term_ui::try_dispatch_shell(trimmed, &split, &mut term_state, &mut session)
            {
                Ok(crate::term_ui::DispatchOutcome::Continue) => continue,
                Ok(crate::term_ui::DispatchOutcome::Exit) => break,
                Ok(crate::term_ui::DispatchOutcome::PassToKovrek) => {}
                Err(e) => {
                    eprintln!("error: {e}");
                    term_state.last_exit = 1;
                    continue;
                }
            }
        } else {
            term_state.push_history(raw_line.trim());
        }

        buffer.push_str(&raw_line);
        buffer.push('\n');
        if unclosed_delimiters(&buffer) > 0 {
            continue;
        }
        match session.eval_repl(&buffer) {
            Ok(Some(v)) => {
                println!("{v}");
                term_state.last_exit = 0;
            }
            Ok(None) => {
                term_state.last_exit = 0;
            }
            Err(e) => {
                eprintln!("error: {e}");
                term_state.last_exit = 1;
            }
        }
        buffer.clear();
    }
    Ok(())
}

pub(crate) fn usage() {
    println!(
        "Kovrek {VERSION}\n\nUsage:\n  kovrek run [--caps c1,c2] [file.kvr]\n  kovrek launch <file.kvr|.kvbc>\n  kovrek bundle [--gui] <script.kvr> [output]\n  kovrek check [file.kvr]\n  kovrek verify [file.kvr]\n  kovrek lint [file.kvr]\n  kovrek debug [file.kvr]\n  kovrek dap\n  kovrek ast [file.kvr]\n  kovrek symbols [file.kvr]\n  kovrek lsp\n  kovrek test\n  kovrek bench [file.kvr] [iterations]\n  kovrek profile [file.kvr]\n  kovrek doc [file.kvr]\n  kovrek aot [--direct] [--elf|--pe] [file.kvr] [out_exe]\n  kovrek wasm [--wasi] [file.kvr] [-o out.wasm] [--disasm] [--run <fn> [args...]]\n  kovrek ffi bindgen <header.h> <lib_path> [-o out.kvr]\n  kovrek notebook [dir] [port]\n  kovrek build [--native]\n  kovrek audit\n  kovrek serve [dir] [port]\n  kovrek web <routes.json> [port]\n  kovrek app <app.kvr> <routes.json> [port]\n  kovrek disasm [file.kvr]\n  kovrek fmt <file.kvr>\n  kovrek new <name>\n  kovrek pkg list\n  kovrek pkg search <term>\n  kovrek pkg info <name>\n  kovrek pkg tree\n  kovrek pkg add <name[@semver]>\n  kovrek pkg remove <name>\n  kovrek pkg update [name]\n  kovrek pkg installed\n  kovrek pkg install\n  kovrek pkg publish\n  kovrek pkg keygen [name]\n  kovrek pkg sign [dir] [key]\n  kovrek pkg verify [target]\n  kovrek pkg pack [dir] [out.kvpkg.json]\n  kovrek pkg unpack <bundle.kvpkg.json> [dest]\n  kovrek pkg build-plugin [dir]\n  kovrek pkg py-bridge <pypi_pkg> [out_dir] [--pip-install]\n  kovrek pkg serve [dir] [port]\n  kovrek doctor\n  kovrek terminal | term | repl\n  kovrek --version"
    );
}
