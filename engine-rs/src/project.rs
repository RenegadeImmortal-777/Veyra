// Project scaffolding and build commands, plus the path helpers they share.
//
// Extracted from `main.rs` without behavior change; see docs/ENGINE-SPLIT.md.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value as JsonValue;

use crate::pipeline::{compile_to_vybc, load_source, run_source};

pub(crate) fn fmt_source(src: &str) -> String {
    let mut out = String::new();
    let mut indent = 0usize;
    let mut prev_blank = true;
    for raw in src.lines() {
        let line = raw.trim().to_string();
        if line.is_empty() {
            if !prev_blank {
                out.push('\n');
                prev_blank = true;
            }
            continue;
        }
        prev_blank = false;
        let starts_close = line.starts_with('}');
        if starts_close {
            indent = indent.saturating_sub(1);
        }
        out.push_str(&"    ".repeat(indent));
        out.push_str(&line);
        out.push('\n');
        let mut opens = 0usize;
        let mut closes = 0usize;
        let mut in_str = None;
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            if let Some(q) = in_str {
                if c == '\\' {
                    i += 2;
                    continue;
                }
                if c == q {
                    in_str = None;
                }
            } else if c == '"' || c == '\'' {
                in_str = Some(c);
            } else if (c == '/' && i + 1 < chars.len() && chars[i + 1] == '/') || c == '#' {
                break;
            } else if c == '{' {
                opens += 1;
            } else if c == '}' {
                closes += 1;
            }
            i += 1;
        }
        let extra_closes = closes.saturating_sub(if starts_close { 1 } else { 0 });
        indent = (indent + opens).saturating_sub(extra_closes);
    }
    while out.ends_with("\n\n") {
        out.pop();
    }
    out
}

pub(crate) fn copy_dir_all(src: &Path, dst: &Path) -> Result<(), String> {
    fs::create_dir_all(dst).map_err(|e| e.to_string())?;
    for entry in fs::read_dir(src).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let ty = entry.file_type().map_err(|e| e.to_string())?;
        let target = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

pub(crate) fn project_root(start: &Path) -> Result<PathBuf, String> {
    let mut p = start.canonicalize().unwrap_or_else(|_| start.to_path_buf());
    loop {
        if p.join("veyra.json").exists() {
            return Ok(p);
        }
        if !p.pop() {
            return Err("veyra.json not found".into());
        }
    }
}

pub(crate) fn veyra_home() -> PathBuf {
    if let Ok(v) = env::var("VEYRA_HOME") {
        return PathBuf::from(v);
    }
    if cfg!(target_os = "windows") {
        if let Ok(v) = env::var("LOCALAPPDATA") {
            return PathBuf::from(v).join("Veyra");
        }
    }
    env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

pub(crate) fn project_packages() -> Result<(), String> {
    let root = project_root(&env::current_dir().map_err(|e| e.to_string())?)?;
    let manifest: JsonValue = serde_json::from_str(
        &fs::read_to_string(root.join("veyra.json")).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    println!("Installed Veyra packages");
    if let Some(pkgs) = manifest.get("packages").and_then(JsonValue::as_object) {
        for (name, meta) in pkgs {
            let version = meta
                .get("version")
                .and_then(JsonValue::as_str)
                .unwrap_or("?");
            println!("  {name} {version}");
        }
    }
    Ok(())
}

pub(crate) fn project_entry() -> Result<PathBuf, String> {
    let root = project_root(&env::current_dir().map_err(|e| e.to_string())?)?;
    let manifest: JsonValue = serde_json::from_str(
        &fs::read_to_string(root.join("veyra.json")).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let entry = manifest
        .get("entry")
        .and_then(JsonValue::as_str)
        .unwrap_or("src/main.vy");
    Ok(root.join(entry))
}

pub(crate) fn test_project() -> Result<(), String> {
    let root = project_root(&env::current_dir().map_err(|e| e.to_string())?)?;
    let dir = root.join("tests");
    if !dir.exists() {
        println!("No tests directory");
        return Ok(());
    }
    let mut total = 0usize;
    let mut passed = 0usize;
    for entry in fs::read_dir(&dir).map_err(|e| e.to_string())? {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.extension().and_then(|x| x.to_str()) != Some("vy") {
            continue;
        }
        total += 1;
        match load_source(path.to_string_lossy().as_ref()).and_then(|s| run_source(&s)) {
            Ok(()) => {
                passed += 1;
                println!("PASS {}", path.display());
            }
            Err(e) => println!("FAIL {}: {e}", path.display()),
        }
    }
    println!("{passed}/{total} passed");
    if passed != total {
        return Err("test suite failed".into());
    }
    Ok(())
}

pub(crate) fn build_project(native: bool) -> Result<(), String> {
    let root = project_root(&env::current_dir().map_err(|e| e.to_string())?)?;
    build_project_at(&root, native)
}

pub(crate) fn build_project_at(root: &Path, native: bool) -> Result<(), String> {
    let manifest_path = root.join("veyra.json");
    let manifest: JsonValue =
        serde_json::from_str(&fs::read_to_string(&manifest_path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let name = manifest
        .get("name")
        .and_then(JsonValue::as_str)
        .unwrap_or("app");
    let version = manifest
        .get("version")
        .and_then(JsonValue::as_str)
        .unwrap_or("0.0.0");
    let dist = root.join("dist").join(format!("{name}-{version}"));
    if dist.exists() {
        fs::remove_dir_all(&dist).map_err(|e| e.to_string())?;
    }
    fs::create_dir_all(&dist).map_err(|e| e.to_string())?;
    copy_dir_all(&root.join("src"), &dist.join("src"))?;
    if root.join("vendor").exists() {
        copy_dir_all(&root.join("vendor"), &dist.join("vendor"))?;
    }
    fs::copy(&manifest_path, dist.join("veyra.json")).map_err(|e| e.to_string())?;
    let entry_rel = manifest
        .get("entry")
        .and_then(JsonValue::as_str)
        .unwrap_or("src/main.vy");
    let entry_path = root.join(entry_rel);
    if entry_path.exists() {
        let bundled = load_source(entry_path.to_string_lossy().as_ref())?;
        let vybc = compile_to_vybc(&bundled)?;
        fs::write(dist.join("app.vybc"), vybc).map_err(|e| e.to_string())?;
        if native {
            let bin_name = if cfg!(target_os = "windows") {
                format!("{name}.exe")
            } else {
                name.to_string()
            };
            crate::aot::compile_file_to_native(&entry_path, Some(&dist.join(bin_name)))?;
        }
    } else if native {
        return Err(format!("missing entry file {}", entry_path.display()));
    }
    let exe = env::current_exe().map_err(|e| e.to_string())?;
    fs::copy(
        &exe,
        dist.join(if cfg!(target_os = "windows") {
            "veyra.exe"
        } else {
            "veyra"
        }),
    )
    .map_err(|e| e.to_string())?;
    println!("Built {}", dist.display());
    Ok(())
}
