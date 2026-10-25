#![allow(
    clippy::collapsible_if,
    clippy::needless_range_loop,
    clippy::type_complexity,
    clippy::let_and_return,
    clippy::only_used_in_recursion
)]

use std::{
    collections::HashSet,
    env, fs,
    io::{self, Write},
    path::{Component, Path, PathBuf},
    process::ExitCode,
};

use serde_json::Value as JsonValue;
use sha2::{Digest, Sha256};
use tiny_http::{Header, Response, Server, StatusCode};

const VERSION: &str = "11.0.0-nexus";

mod ast;
mod bytecode;
mod compiler;
mod lexer;
mod lsp;
mod parser;
mod pipeline;
mod runtime;
mod tensor;
mod tooling;
mod value;
mod vm;

#[cfg(test)]
use crate::bytecode::Op;
#[cfg(test)]
use crate::lexer::lex;
use crate::lsp::lsp_server;
#[cfg(test)]
use crate::lsp::{lsp_definition, lsp_references, lsp_rename};
use crate::pipeline::{compile_source, disasm_source, load_source, prepare_vm, run_source};
#[cfg(test)]
use crate::runtime::export_function_object;
use crate::tooling::{
    ast_file, bench_file, debug_file, doc_file, lint_file, profile_file, symbols_file,
};
use crate::value::{Val, map_lookup, val_to_json};
#[cfg(test)]
use crate::vm::Vm;

fn fmt_source(src: &str) -> String {
    let mut out = String::new();
    let mut indent = 0usize;
    for raw in src.lines() {
        let line = raw.trim().to_string();
        if line.starts_with('}') {
            indent = indent.saturating_sub(1);
        }
        if !line.is_empty() {
            out.push_str(&"    ".repeat(indent));
            out.push_str(&line);
            out.push('\n');
        }
        if line.ends_with('{') {
            indent += 1;
        }
    }
    out
}

fn copy_dir_all(src: &Path, dst: &Path) -> Result<(), String> {
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

fn project_root(start: &Path) -> Result<PathBuf, String> {
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

fn veyra_home() -> PathBuf {
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

fn pkg_index() -> Result<(PathBuf, JsonValue), String> {
    if let Ok(url) = env::var("VEYRA_REGISTRY_URL") {
        #[cfg(feature = "native-services")]
        {
            let response = reqwest::blocking::get(&url).map_err(|e| e.to_string())?;
            if !response.status().is_success() {
                return Err(format!(
                    "remote registry returned HTTP {}",
                    response.status()
                ));
            }
            let text = response.text().map_err(|e| e.to_string())?;
            let json = serde_json::from_str(&text).map_err(|e| e.to_string())?;
            return Ok((PathBuf::from(url), json));
        }
        #[cfg(not(feature = "native-services"))]
        {
            return Err(format!(
                "remote registry '{url}' requires the native-services feature"
            ));
        }
    }
    let path = veyra_home().join("registry").join("index.json");
    let text = fs::read_to_string(&path)
        .map_err(|e| format!("registry unavailable at {}: {e}", path.display()))?;
    let json = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    Ok((path, json))
}

fn safe_package_relative_path(value: &str) -> Result<PathBuf, String> {
    let path = Path::new(value);
    if path.is_absolute()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(format!("unsafe package path '{value}'"));
    }
    Ok(path.to_path_buf())
}

#[cfg(feature = "native-services")]
fn fetch_package_file(url: &str) -> Result<Vec<u8>, String> {
    let response = reqwest::blocking::get(url).map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!(
            "package download returned HTTP {}",
            response.status()
        ));
    }
    response
        .bytes()
        .map(|b| b.to_vec())
        .map_err(|e| e.to_string())
}

fn hash_directory(dir: &Path) -> Result<String, String> {
    fn collect(base: &Path, dir: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
        for entry in fs::read_dir(dir).map_err(|e| e.to_string())? {
            let path = entry.map_err(|e| e.to_string())?.path();
            if path.is_dir() {
                collect(base, &path, files)?;
            } else {
                files.push(path);
            }
        }
        Ok(())
    }
    if !dir.exists() {
        return Err(format!("missing directory {}", dir.display()));
    }
    let mut files = vec![];
    collect(dir, dir, &mut files)?;
    files.sort();
    let mut h = Sha256::new();
    for path in files {
        let rel = path
            .strip_prefix(dir)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        h.update(rel.as_bytes());
        h.update([0]);
        h.update(fs::read(&path).map_err(|e| e.to_string())?);
        h.update([0]);
    }
    Ok(format!("{:x}", h.finalize()))
}

fn write_package_lock(root: &Path, manifest: &JsonValue) -> Result<(), String> {
    let mut locked = serde_json::Map::new();
    if let Some(packages) = manifest.get("packages").and_then(JsonValue::as_object) {
        for (name, meta) in packages {
            let mut entry = meta.clone();
            let hash = hash_directory(&root.join("vendor").join(name))?;
            entry["sha256"] = JsonValue::String(hash);
            locked.insert(name.clone(), entry);
        }
    }
    let lock = serde_json::json!({
        "format": 2,
        "engine": VERSION,
        "packages": JsonValue::Object(locked)
    });
    fs::write(
        root.join("veyra.lock"),
        serde_json::to_string_pretty(&lock).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())
}

fn pkg_add_inner(name: &str, seen: &mut HashSet<String>) -> Result<(), String> {
    if !seen.insert(name.to_string()) {
        return Ok(());
    }
    let root = project_root(&env::current_dir().map_err(|e| e.to_string())?)?;
    let (index_path, index) = pkg_index()?;
    let pkg = index
        .get("packages")
        .and_then(|x| x.get(name))
        .ok_or(format!("package '{name}' not found in Veyra registry"))?;
    let version = pkg
        .get("version")
        .and_then(JsonValue::as_str)
        .unwrap_or("0.0.0");
    let entry = pkg
        .get("entry")
        .and_then(JsonValue::as_str)
        .unwrap_or("main.vy");
    if let Some(dependencies) = pkg.get("dependencies").and_then(JsonValue::as_array) {
        for dependency in dependencies {
            let dependency = dependency
                .as_str()
                .ok_or("package dependency names must be Strings")?;
            pkg_add_inner(dependency, seen)?;
        }
    }
    let dest = root.join("vendor").join(name);
    if dest.exists() {
        fs::remove_dir_all(&dest).map_err(|e| e.to_string())?;
    }

    if let Some(files) = pkg.get("files").and_then(JsonValue::as_object) {
        fs::create_dir_all(&dest).map_err(|e| e.to_string())?;
        #[cfg(feature = "native-services")]
        {
            for (relative, url) in files {
                let relative = safe_package_relative_path(relative)?;
                let url = url
                    .as_str()
                    .ok_or("registry package file URL must be String")?;
                let target = dest.join(relative);
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                fs::write(&target, fetch_package_file(url)?).map_err(|e| e.to_string())?;
            }
        }
        #[cfg(not(feature = "native-services"))]
        {
            let _ = files;
            return Err("remote package files require the native-services feature".into());
        }
    } else if let Some(url) = pkg.get("url").and_then(JsonValue::as_str) {
        fs::create_dir_all(&dest).map_err(|e| e.to_string())?;
        let relative = safe_package_relative_path(entry)?;
        let target = dest.join(relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        #[cfg(feature = "native-services")]
        {
            fs::write(&target, fetch_package_file(url)?).map_err(|e| e.to_string())?;
        }
        #[cfg(not(feature = "native-services"))]
        {
            let _ = url;
            return Err("remote package download requires the native-services feature".into());
        }
    } else {
        let source = pkg
            .get("source")
            .and_then(JsonValue::as_str)
            .ok_or("registry package requires source, url, or files")?;
        let source_path = index_path.parent().unwrap_or(Path::new(".")).join(source);
        copy_dir_all(&source_path, &dest)?;
    }

    let manifest_path = root.join("veyra.json");
    let mut manifest: JsonValue =
        serde_json::from_str(&fs::read_to_string(&manifest_path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    if !manifest
        .get("packages")
        .map(JsonValue::is_object)
        .unwrap_or(false)
    {
        manifest["packages"] = JsonValue::Object(serde_json::Map::new());
    }
    manifest["packages"][name] = serde_json::json!({"version":version,"entry":entry});
    fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    write_package_lock(&root, &manifest)?;
    println!("Added {name} {version} -> vendor/{name}/{entry}");
    Ok(())
}

fn pkg_add(name: &str) -> Result<(), String> {
    pkg_add_inner(name, &mut HashSet::new())
}

fn pkg_remove(name: &str) -> Result<(), String> {
    let root = project_root(&env::current_dir().map_err(|e| e.to_string())?)?;
    let dest = root.join("vendor").join(name);
    if dest.exists() {
        fs::remove_dir_all(&dest).map_err(|e| e.to_string())?;
    }
    let manifest_path = root.join("veyra.json");
    let mut manifest: JsonValue =
        serde_json::from_str(&fs::read_to_string(&manifest_path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    if let Some(obj) = manifest
        .get_mut("packages")
        .and_then(JsonValue::as_object_mut)
    {
        obj.remove(name);
    }
    fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    write_package_lock(&root, &manifest)?;
    println!("Removed {name}");
    Ok(())
}

fn pkg_list() -> Result<(), String> {
    let (_, index) = pkg_index()?;
    println!("Veyra Registry");
    if let Some(pkgs) = index.get("packages").and_then(JsonValue::as_object) {
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

fn pkg_install() -> Result<(), String> {
    let root = project_root(&env::current_dir().map_err(|e| e.to_string())?)?;
    let manifest: JsonValue = serde_json::from_str(
        &fs::read_to_string(root.join("veyra.json")).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let names: Vec<String> = manifest
        .get("packages")
        .and_then(JsonValue::as_object)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    for name in names {
        pkg_add(&name)?;
    }
    println!("Veyra packages restored");
    Ok(())
}

fn pkg_search(term: &str) -> Result<(), String> {
    let (_, index) = pkg_index()?;
    println!("Veyra Registry search: {term}");
    if let Some(pkgs) = index.get("packages").and_then(JsonValue::as_object) {
        for (name, meta) in pkgs {
            if name.to_lowercase().contains(&term.to_lowercase()) {
                let version = meta
                    .get("version")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("?");
                println!("  {name} {version}");
            }
        }
    }
    Ok(())
}

fn pkg_publish() -> Result<(), String> {
    let root = project_root(&env::current_dir().map_err(|e| e.to_string())?)?;
    let manifest_path = root.join("veyra.json");
    let manifest: JsonValue =
        serde_json::from_str(&fs::read_to_string(&manifest_path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let name = manifest
        .get("name")
        .and_then(JsonValue::as_str)
        .ok_or("manifest missing name")?;
    let version = manifest
        .get("version")
        .and_then(JsonValue::as_str)
        .unwrap_or("0.0.0");
    let entry = manifest
        .get("entry")
        .and_then(JsonValue::as_str)
        .unwrap_or("src/main.vy");

    let home = veyra_home();
    let pkg_root = home.join("packages").join(name);
    if pkg_root.exists() {
        fs::remove_dir_all(&pkg_root).map_err(|e| e.to_string())?;
    }
    fs::create_dir_all(&pkg_root).map_err(|e| e.to_string())?;
    copy_dir_all(&root.join("src"), &pkg_root.join("src"))?;

    let index_path = home.join("registry").join("index.json");
    fs::create_dir_all(index_path.parent().unwrap_or(Path::new("."))).map_err(|e| e.to_string())?;
    let mut index: JsonValue = if index_path.exists() {
        serde_json::from_str(&fs::read_to_string(&index_path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?
    } else {
        serde_json::json!({"registry_version":1,"packages":{}})
    };
    if !index
        .get("packages")
        .map(JsonValue::is_object)
        .unwrap_or(false)
    {
        index["packages"] = JsonValue::Object(serde_json::Map::new());
    }
    index["packages"][name] = serde_json::json!({
        "version": version,
        "entry": entry,
        "source": format!("../packages/{name}")
    });
    fs::write(
        &index_path,
        serde_json::to_string_pretty(&index).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    println!("Published {name} {version} to {}", index_path.display());
    Ok(())
}

fn pkg_audit() -> Result<(), String> {
    let root = project_root(&env::current_dir().map_err(|e| e.to_string())?)?;
    let lock_path = root.join("veyra.lock");
    if !lock_path.exists() {
        return Err("veyra.lock not found; run veyra install/add first".into());
    }
    let lock: JsonValue =
        serde_json::from_str(&fs::read_to_string(&lock_path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let packages = lock
        .get("packages")
        .and_then(JsonValue::as_object)
        .ok_or("invalid lockfile packages")?;
    let mut checked = 0usize;
    for (name, meta) in packages {
        let expected = meta
            .get("sha256")
            .and_then(JsonValue::as_str)
            .ok_or(format!("package {name} has no sha256"))?;
        let actual = hash_directory(&root.join("vendor").join(name))?;
        if actual != expected {
            return Err(format!("package integrity mismatch: {name}"));
        }
        checked += 1;
    }
    println!("Audit OK: {checked} package(s), content hashes verified");
    Ok(())
}

fn project_packages() -> Result<(), String> {
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

fn project_entry() -> Result<PathBuf, String> {
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

fn test_project() -> Result<(), String> {
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

fn build_project() -> Result<(), String> {
    let root = project_root(&env::current_dir().map_err(|e| e.to_string())?)?;
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

fn serve_directory(dir: &Path, port: u16) -> Result<(), String> {
    let server = Server::http(format!("0.0.0.0:{port}")).map_err(|e| e.to_string())?;
    println!("Veyra web server: http://127.0.0.1:{port}");
    println!("Serving {}", dir.display());
    for req in server.incoming_requests() {
        let raw = req.url().split('?').next().unwrap_or("/");
        let rel = raw.trim_start_matches('/');
        let mut path = dir.join(if rel.is_empty() { "index.html" } else { rel });
        if path.is_dir() {
            path = path.join("index.html");
        }
        let response = if path.exists() && path.is_file() {
            let data = fs::read(&path).map_err(|e| e.to_string())?;
            let mut r = Response::from_data(data);
            let content_type = match path.extension().and_then(|x| x.to_str()).unwrap_or("") {
                "html" => "text/html; charset=utf-8",
                "css" => "text/css; charset=utf-8",
                "js" => "application/javascript; charset=utf-8",
                "json" => "application/json; charset=utf-8",
                "png" => "image/png",
                "jpg" | "jpeg" => "image/jpeg",
                "svg" => "image/svg+xml",
                _ => "application/octet-stream",
            };
            if let Ok(h) = Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes()) {
                r.add_header(h);
            }
            r
        } else {
            Response::from_string("404 Not Found").with_status_code(StatusCode(404))
        };
        req.respond(response).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn serve_routes(routes_file: &Path, port: u16) -> Result<(), String> {
    let config: JsonValue =
        serde_json::from_str(&fs::read_to_string(routes_file).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let routes = config
        .get("routes")
        .and_then(JsonValue::as_object)
        .ok_or("web route file requires a 'routes' object")?;
    let server = Server::http(format!("0.0.0.0:{port}")).map_err(|e| e.to_string())?;
    println!("Veyra API server: http://127.0.0.1:{port}");
    println!("Routes: {}", routes.len());
    for mut req in server.incoming_requests() {
        let path = req.url().split('?').next().unwrap_or("/");
        let method = req.method().as_str();
        let method_key = format!("{method} {path}");
        let route = routes.get(&method_key).or_else(|| routes.get(path));
        let response = if let Some(route) = route {
            let status = route
                .get("status")
                .and_then(JsonValue::as_u64)
                .unwrap_or(200) as u16;
            let content_type = route
                .get("content_type")
                .and_then(JsonValue::as_str)
                .unwrap_or_else(|| {
                    if route.get("json").is_some() {
                        "application/json; charset=utf-8"
                    } else {
                        "text/plain; charset=utf-8"
                    }
                });
            let body = if route
                .get("echo")
                .and_then(JsonValue::as_bool)
                .unwrap_or(false)
            {
                let mut text = String::new();
                req.as_reader()
                    .read_to_string(&mut text)
                    .map_err(|e| e.to_string())?;
                text
            } else if let Some(j) = route.get("json") {
                serde_json::to_string(j).map_err(|e| e.to_string())?
            } else {
                route
                    .get("body")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("")
                    .to_string()
            };
            let mut r = Response::from_string(body).with_status_code(StatusCode(status));
            if let Ok(h) = Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes()) {
                r.add_header(h);
            }
            r
        } else {
            Response::from_string("404 Not Found").with_status_code(StatusCode(404))
        };
        req.respond(response).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn match_route_pattern(pattern: &str, path: &str) -> Option<Vec<(String, String)>> {
    let p: Vec<&str> = pattern
        .trim_matches('/')
        .split('/')
        .filter(|x| !x.is_empty())
        .collect();
    let a: Vec<&str> = path
        .trim_matches('/')
        .split('/')
        .filter(|x| !x.is_empty())
        .collect();
    if p.len() != a.len() {
        return None;
    }
    let mut params = vec![];
    for (expected, actual) in p.iter().zip(a.iter()) {
        if expected.starts_with('{') && expected.ends_with('}') && expected.len() > 2 {
            params.push((
                expected[1..expected.len() - 1].to_string(),
                (*actual).to_string(),
            ));
        } else if expected != actual {
            return None;
        }
    }
    Some(params)
}

fn query_params(url: &str) -> Val {
    let mut pairs = vec![];
    if let Some(query) = url.split_once('?').map(|(_, q)| q) {
        for item in query.split('&').filter(|x| !x.is_empty()) {
            let (key, value) = item.split_once('=').unwrap_or((item, ""));
            pairs.push((Val::Str(key.to_string()), Val::Str(value.to_string())));
        }
    }
    Val::Map(pairs)
}

fn serve_veyra_app(app_file: &Path, routes_file: &Path, port: u16) -> Result<(), String> {
    let src = load_source(app_file.to_string_lossy().as_ref())?;
    let vm = prepare_vm(&src)?;
    let config: JsonValue =
        serde_json::from_str(&fs::read_to_string(routes_file).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let routes = config
        .get("routes")
        .and_then(JsonValue::as_object)
        .ok_or("app routes file requires a 'routes' object")?;
    let server = Server::http(format!("0.0.0.0:{port}")).map_err(|e| e.to_string())?;
    println!("Veyra dynamic app: http://127.0.0.1:{port}");
    println!("Handlers: {}", routes.len());
    for mut req in server.incoming_requests() {
        let full_url = req.url().to_string();
        let path = full_url.split('?').next().unwrap_or("/").to_string();
        let method = req.method().as_str().to_string();
        let key = format!("{method} {path}");
        let mut route_params: Vec<(String, String)> = vec![];
        let mut handler = routes
            .get(&key)
            .or_else(|| routes.get(&path))
            .and_then(JsonValue::as_str)
            .map(str::to_string);
        if handler.is_none() {
            for (route_key, value) in routes {
                let (route_method, pattern) = route_key
                    .split_once(' ')
                    .map(|(m, p)| (Some(m), p))
                    .unwrap_or((None, route_key.as_str()));
                if route_method.is_some() && route_method != Some(method.as_str()) {
                    continue;
                }
                if let Some(params) = match_route_pattern(pattern, &path) {
                    if let Some(name) = value.as_str() {
                        handler = Some(name.to_string());
                        route_params = params;
                        break;
                    }
                }
            }
        }
        let response = if let Some(handler) = handler {
            let mut body = String::new();
            req.as_reader()
                .read_to_string(&mut body)
                .map_err(|e| e.to_string())?;
            let params = Val::Map(
                route_params
                    .iter()
                    .map(|(k, v)| (Val::Str(k.clone()), Val::Str(v.clone())))
                    .collect(),
            );
            let query = query_params(&full_url);
            let headers = Val::Map(
                req.headers()
                    .iter()
                    .map(|h| {
                        (
                            Val::Str(h.field.as_str().to_string()),
                            Val::Str(h.value.as_str().to_string()),
                        )
                    })
                    .collect(),
            );
            let request = Val::Map(vec![
                (Val::Str("method".into()), Val::Str(method)),
                (Val::Str("path".into()), Val::Str(path)),
                (Val::Str("url".into()), Val::Str(full_url)),
                (Val::Str("body".into()), Val::Str(body)),
                (Val::Str("params".into()), params),
                (Val::Str("query".into()), query),
                (Val::Str("headers".into()), headers),
            ]);
            match vm.invoke(&handler, vec![request]) {
                Ok(value) => {
                    let (status, content_type, body, response_headers) = match value {
                        Val::Map(pairs) => {
                            let status = map_lookup(&pairs, &Val::Str("status".into()))
                                .and_then(|v| {
                                    if let Val::Num(n) = v {
                                        Some(*n as u16)
                                    } else {
                                        None
                                    }
                                })
                                .unwrap_or(200);
                            let content_type = map_lookup(&pairs, &Val::Str("content_type".into()))
                                .and_then(|v| {
                                    if let Val::Str(s) = v {
                                        Some(s.clone())
                                    } else {
                                        None
                                    }
                                })
                                .unwrap_or_else(|| "application/json; charset=utf-8".into());
                            let body = map_lookup(&pairs, &Val::Str("body".into()))
                                .cloned()
                                .unwrap_or(Val::Null);
                            let text = match body {
                                Val::Str(s) => s,
                                other => serde_json::to_string(&val_to_json(&other))
                                    .map_err(|e| e.to_string())?,
                            };
                            let headers = match map_lookup(&pairs, &Val::Str("headers".into())) {
                                Some(Val::Map(items)) => items
                                    .iter()
                                    .filter_map(|(k, v)| match (k, v) {
                                        (Val::Str(name), Val::Str(value)) => {
                                            Some((name.clone(), value.clone()))
                                        }
                                        _ => None,
                                    })
                                    .collect::<Vec<_>>(),
                                _ => vec![],
                            };
                            (status, content_type, text, headers)
                        }
                        Val::Str(s) => (200, "text/plain; charset=utf-8".into(), s, vec![]),
                        other => (
                            200,
                            "application/json; charset=utf-8".into(),
                            serde_json::to_string(&val_to_json(&other))
                                .map_err(|e| e.to_string())?,
                            vec![],
                        ),
                    };
                    let mut r = Response::from_string(body).with_status_code(StatusCode(status));
                    if let Ok(h) = Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes())
                    {
                        r.add_header(h);
                    }
                    for (name, value) in response_headers {
                        if let Ok(h) = Header::from_bytes(name.as_bytes(), value.as_bytes()) {
                            r.add_header(h);
                        }
                    }
                    r
                }
                Err(e) => Response::from_string(format!("Veyra handler error: {e}"))
                    .with_status_code(StatusCode(500)),
            }
        } else {
            Response::from_string("404 Not Found").with_status_code(StatusCode(404))
        };
        req.respond(response).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn new_project(name: &str) -> Result<(), String> {
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

fn doctor() {
    println!("Veyra {VERSION}");
    println!("engine: native Rust bytecode VM");
    println!("target: {}", env::consts::ARCH);
    println!("os: {}", env::consts::OS);
    println!("status: OK");
}

fn repl() -> Result<(), String> {
    println!("Veyra {VERSION} native REPL Ã¢â‚¬â€ :quit to exit");
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

fn usage() {
    println!(
        "Veyra {VERSION}\n\nUsage:\n  veyra run [file.vy]\n  veyra check [file.vy]\n  veyra lint [file.vy]\n  veyra debug [file.vy]\n  veyra ast [file.vy]\n  veyra symbols [file.vy]\n  veyra lsp\n  veyra test\n  veyra bench [file.vy] [iterations]\n  veyra profile [file.vy]\n  veyra doc [file.vy]\n  veyra build\n  veyra audit\n  veyra serve [dir] [port]\n  veyra web <routes.json> [port]\n  veyra app <app.vy> <routes.json> [port]\n  veyra disasm [file.vy]\n  veyra fmt <file.vy>\n  veyra new <name>\n  veyra pkg list\n  veyra pkg add <name>\n  veyra pkg remove <name>\n  veyra pkg installed\n  veyra doctor\n  veyra repl\n  veyra --version"
    );
}
fn main() -> ExitCode {
    let a: Vec<String> = env::args().collect();
    let r: Result<(), String> = match a.get(1).map(String::as_str) {
        Some("--version") | Some("version") => {
            println!("Veyra {VERSION}");
            Ok(())
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
                "usage: veyra pkg <list|search|installed|install|publish|add|remove> [name]".into(),
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
        Some("run") => {
            let path = match a.get(2) {
                Some(p) => Ok(PathBuf::from(p)),
                None => project_entry(),
            };
            path.and_then(|p| load_source(p.to_string_lossy().as_ref()))
                .and_then(|s| run_source(&s))
        }
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
        Some("build") => build_project(),
        Some("audit") => pkg_audit(),
        Some("serve") => {
            let dir = a
                .get(2)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            let port = a.get(3).and_then(|x| x.parse::<u16>().ok()).unwrap_or(8080);
            serve_directory(&dir, port)
        }
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
}
