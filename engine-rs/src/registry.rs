// Package registry: the project index, integrity hashing, the lockfile and the
// add/remove/list/install/search/publish/audit commands.
//
// Extracted from `main.rs` without behavior change; see docs/ENGINE-SPLIT.md.

use std::collections::{BTreeMap, HashSet};
use std::env;
use std::fs;
use std::path::{Component, Path, PathBuf};

use base64::Engine;
use rand::Rng;
use serde_json::Value as JsonValue;
use sha2::{Digest, Sha256};

use crate::VERSION;
use crate::project::{copy_dir_all, kovrek_home, project_root};
use crate::value::{constant_time_strings_equal, hmac_sha256_hex};

fn cache_key(input: &str) -> String {
    let mut h = Sha256::new();
    h.update(input.as_bytes());
    let full = format!("{:x}", h.finalize());
    full[..24].to_string()
}

fn registry_cache_dir() -> PathBuf {
    kovrek_home().join("cache").join("registries")
}

fn artifact_cache_dir() -> PathBuf {
    kovrek_home().join("cache").join("artifacts")
}

fn offline_mode() -> bool {
    env::var("KOVREK_OFFLINE")
        .ok()
        .map(|v| matches!(v.trim(), "1" | "true" | "TRUE" | "yes"))
        .unwrap_or(false)
}

#[cfg(feature = "native-services")]
fn http_get_bytes(url: &str) -> Result<Vec<u8>, String> {
    let response = reqwest::blocking::get(url).map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!(
            "HTTP request to {url} returned status {}",
            response.status()
        ));
    }
    response
        .bytes()
        .map(|b| b.to_vec())
        .map_err(|e| e.to_string())
}

#[cfg(not(feature = "native-services"))]
fn http_get_bytes(url: &str) -> Result<Vec<u8>, String> {
    Err(format!(
        "remote URL '{url}' requires the native-services feature"
    ))
}

fn http_get_text(url: &str) -> Result<String, String> {
    let bytes = http_get_bytes(url)?;
    String::from_utf8(bytes).map_err(|e| format!("invalid UTF-8 from {url}: {e}"))
}

/// Ordered registry URLs or local paths from `KOVREK_REGISTRY_URLS`,
/// `KOVREK_REGISTRY_URL`, or the default local `$KOVREK_HOME/registry/index.json`.
pub(crate) fn registry_sources() -> Vec<String> {
    if let Ok(urls) = env::var("KOVREK_REGISTRY_URLS") {
        let list: Vec<String> = urls
            .split([',', ';'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        if !list.is_empty() {
            return list;
        }
    }
    if let Ok(url) = env::var("KOVREK_REGISTRY_URL") {
        let trimmed = url.trim();
        if !trimmed.is_empty() {
            return vec![trimmed.to_string()];
        }
    }
    vec![
        kovrek_home()
            .join("registry")
            .join("index.json")
            .to_string_lossy()
            .into_owned(),
    ]
}

fn file_url_to_path(url: &str) -> PathBuf {
    let raw = if let Some(rest) = url.strip_prefix("file:///") {
        #[cfg(windows)]
        {
            rest.replace('/', "\\")
        }
        #[cfg(not(windows))]
        {
            format!("/{rest}")
        }
    } else if let Some(rest) = url.strip_prefix("file://") {
        rest.to_string()
    } else {
        url.to_string()
    };
    PathBuf::from(raw)
}

fn load_single_registry(source: &str) -> Result<(PathBuf, String, JsonValue), String> {
    if source.starts_with("http://") || source.starts_with("https://") {
        let target_url = if source.ends_with(".json") {
            source.to_string()
        } else {
            format!("{}/index.json", source.trim_end_matches('/'))
        };
        let base_url = if let Some(stripped) = target_url.strip_suffix("/index.json") {
            stripped.to_string()
        } else if let Some((parent, _)) = target_url.rsplit_once('/') {
            parent.to_string()
        } else {
            target_url.clone()
        };
        let cache_path = registry_cache_dir().join(format!("{}.json", cache_key(&target_url)));
        let text = if offline_mode() {
            fs::read_to_string(&cache_path)
                .map_err(|e| format!("offline cache miss for {target_url}: {e}"))?
        } else {
            match http_get_text(&target_url) {
                Ok(t) => {
                    if let Some(parent) = cache_path.parent() {
                        let _ = fs::create_dir_all(parent);
                    }
                    let _ = fs::write(&cache_path, &t);
                    t
                }
                Err(err) => {
                    if cache_path.is_file() {
                        fs::read_to_string(&cache_path).map_err(|_| err)?
                    } else {
                        return Err(err);
                    }
                }
            }
        };
        let json: JsonValue = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        return Ok((PathBuf::from(target_url), base_url, json));
    }
    let mut path = file_url_to_path(source);
    if path.is_dir() {
        path = path.join("index.json");
    }
    let text = fs::read_to_string(&path)
        .map_err(|e| format!("registry unavailable at {}: {e}", path.display()))?;
    let json: JsonValue = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let base_dir = path
        .parent()
        .unwrap_or(Path::new("."))
        .to_string_lossy()
        .into_owned();
    Ok((path, base_dir, json))
}

fn resolve_pkg_ref(base: &str, rel_or_url: &str) -> String {
    if rel_or_url.starts_with("http://")
        || rel_or_url.starts_with("https://")
        || rel_or_url.starts_with("file://")
    {
        return rel_or_url.to_string();
    }
    if base.starts_with("http://") || base.starts_with("https://") {
        return format!(
            "{}/{}",
            base.trim_end_matches('/'),
            rel_or_url.trim_start_matches('/')
        );
    }
    if let Ok(rel_path) = safe_package_relative_path(rel_or_url) {
        return Path::new(base)
            .join(rel_path)
            .to_string_lossy()
            .into_owned();
    }
    Path::new(base)
        .join(rel_or_url)
        .to_string_lossy()
        .into_owned()
}

pub(crate) fn pkg_index() -> Result<(PathBuf, JsonValue), String> {
    let sources = registry_sources();
    let mut merged = serde_json::Map::new();
    let mut first_path: Option<PathBuf> = None;
    let mut registry_version = 1u64;
    let mut errors = Vec::new();
    for src in &sources {
        match load_single_registry(src) {
            Ok((idx_path, base, json)) => {
                if first_path.is_none() {
                    first_path = Some(idx_path.clone());
                    if let Some(rv) = json.get("registry_version").and_then(JsonValue::as_u64) {
                        registry_version = rv;
                    }
                }
                if let Some(pkgs) = json.get("packages").and_then(JsonValue::as_object) {
                    for (name, meta) in pkgs {
                        if merged.contains_key(name) {
                            continue;
                        }
                        let mut annotated = meta.clone();
                        if let Some(obj) = annotated.as_object_mut() {
                            obj.insert("_registry_base".into(), JsonValue::String(base.clone()));
                            obj.insert(
                                "_registry_index".into(),
                                JsonValue::String(idx_path.to_string_lossy().into_owned()),
                            );
                        }
                        merged.insert(name.clone(), annotated);
                    }
                }
            }
            Err(e) => errors.push(format!("{src}: {e}")),
        }
    }
    if let Some(path) = first_path {
        Ok((
            path,
            serde_json::json!({
                "registry_version": registry_version,
                "packages": JsonValue::Object(merged),
            }),
        ))
    } else {
        Err(errors.join("; "))
    }
}

pub(crate) fn safe_package_relative_path(value: &str) -> Result<PathBuf, String> {
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

pub(crate) fn fetch_package_file(url: &str) -> Result<Vec<u8>, String> {
    if url.starts_with("http://") || url.starts_with("https://") {
        let cache_path = artifact_cache_dir().join(format!("{}.bin", cache_key(url)));
        if offline_mode() {
            return fs::read(&cache_path).map_err(|e| format!("offline cache miss for {url}: {e}"));
        }
        return match http_get_bytes(url) {
            Ok(bytes) => {
                if let Some(parent) = cache_path.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                let _ = fs::write(&cache_path, &bytes);
                Ok(bytes)
            }
            Err(err) => {
                if cache_path.is_file() {
                    fs::read(&cache_path).map_err(|_| err)
                } else {
                    Err(err)
                }
            }
        };
    }
    let path = file_url_to_path(url);
    fs::read(&path).map_err(|e| format!("failed to read {}: {e}", path.display()))
}

pub(crate) fn hash_directory(dir: &Path) -> Result<String, String> {
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

fn parse_semver_tuple(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.trim().trim_start_matches('v').split(['-', '+']).next()?;
    let parts: Vec<&str> = core.split('.').collect();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    let maj = parts[0].parse::<u64>().ok()?;
    let min = parts
        .get(1)
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    let pat = parts
        .get(2)
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    Some((maj, min, pat))
}

pub(crate) fn semver_matches(version: &str, constraint: &str) -> bool {
    let req = constraint.trim();
    if req.is_empty() || req == "*" {
        return true;
    }
    let Some((maj, min, pat)) = parse_semver_tuple(version) else {
        return version == req;
    };
    let raw_tokens: Vec<&str> = req
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|s| !s.is_empty())
        .collect();
    let mut clauses: Vec<String> = Vec::with_capacity(raw_tokens.len());
    let mut i = 0;
    while i < raw_tokens.len() {
        let tok = raw_tokens[i];
        if matches!(tok, "^" | "~" | ">=" | "<=" | ">" | "<" | "=") && i + 1 < raw_tokens.len() {
            clauses.push(format!("{tok}{}", raw_tokens[i + 1]));
            i += 2;
        } else {
            clauses.push(tok.to_string());
            i += 1;
        }
    }
    for clause in clauses {
        let ok = if let Some(rest) = clause.strip_prefix("^") {
            let Some((rmaj, rmin, rpat)) = parse_semver_tuple(rest) else {
                return false;
            };
            let lower = (maj, min, pat) >= (rmaj, rmin, rpat);
            let upper = if rmaj > 0 {
                maj == rmaj
            } else if rmin > 0 {
                maj == 0 && min == rmin
            } else {
                maj == 0 && min == 0 && pat == rpat
            };
            lower && upper
        } else if let Some(rest) = clause.strip_prefix("~") {
            let Some((rmaj, rmin, rpat)) = parse_semver_tuple(rest) else {
                return false;
            };
            maj == rmaj && min == rmin && pat >= rpat
        } else if let Some(rest) = clause.strip_prefix(">=") {
            parse_semver_tuple(rest).is_some_and(|r| (maj, min, pat) >= r)
        } else if let Some(rest) = clause.strip_prefix("<=") {
            parse_semver_tuple(rest).is_some_and(|r| (maj, min, pat) <= r)
        } else if let Some(rest) = clause.strip_prefix('>') {
            parse_semver_tuple(rest).is_some_and(|r| (maj, min, pat) > r)
        } else if let Some(rest) = clause.strip_prefix('<') {
            parse_semver_tuple(rest).is_some_and(|r| (maj, min, pat) < r)
        } else {
            let clean = clause.strip_prefix('=').unwrap_or(&clause);
            parse_semver_tuple(clean).is_some_and(|r| (maj, min, pat) == r)
        };
        if !ok {
            return false;
        }
    }
    true
}

/// Validate an unscoped (`foo`) or scoped (`@org/pkg`) Kovrek package name.
pub(crate) fn validate_package_name(name: &str) -> Result<(), String> {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.contains('\\') {
        return Err(format!("unsafe package name '{name}'"));
    }
    if let Some(rest) = trimmed.strip_prefix('@') {
        let mut parts = rest.split('/');
        let scope = parts.next().unwrap_or("");
        let pkg = parts.next().unwrap_or("");
        if parts.next().is_some()
            || scope.is_empty()
            || pkg.is_empty()
            || scope == "."
            || scope == ".."
            || pkg == "."
            || pkg == ".."
            || scope.starts_with('.')
            || pkg.starts_with('.')
        {
            return Err(format!("invalid scoped package name '{name}'"));
        }
        return Ok(());
    }
    if trimmed.contains('/') || trimmed == "." || trimmed == ".." || trimmed.starts_with('.') {
        return Err(format!("invalid package name '{name}'"));
    }
    Ok(())
}

/// Split `name[@constraint]`, supporting scoped names like `@org/pkg@^1.2.0`.
pub(crate) fn parse_pkg_spec(spec: &str) -> Result<(&str, Option<&str>), String> {
    let s = spec.trim();
    if s.is_empty() {
        return Err("package name cannot be empty".into());
    }
    if let Some(rest) = s.strip_prefix('@') {
        if let Some(slash) = rest.find('/') {
            let after_slash = &rest[slash + 1..];
            if after_slash.is_empty() {
                return Err(format!("invalid scoped package spec '{s}'"));
            }
            if let Some(at_rel) = after_slash.find('@') {
                let split_idx = 1 + slash + 1 + at_rel;
                let (name, ver) = s.split_at(split_idx);
                let constraint = ver.trim_start_matches('@').trim();
                return Ok((
                    name,
                    if constraint.is_empty() {
                        None
                    } else {
                        Some(constraint)
                    },
                ));
            }
            return Ok((s, None));
        }
        return Err(format!("scoped package must be '@scope/name', got '{s}'"));
    }
    Ok(match s.split_once('@') {
        Some((n, c)) => {
            let c = c.trim();
            (n.trim(), if c.is_empty() { None } else { Some(c) })
        }
        None => (s, None),
    })
}

/// Resolve `vendor/<name>` safely, mapping `@org/pkg` to `vendor/@org/pkg`.
pub(crate) fn vendor_pkg_dir(root: &Path, name: &str) -> PathBuf {
    let mut dir = root.join("vendor");
    for seg in name.split('/').filter(|s| !s.is_empty()) {
        dir = dir.join(seg);
    }
    dir
}

/// Current host OS + architecture key used for platform-specific native plugins.
pub(crate) fn current_platform_key() -> String {
    format!("{}-{}", env::consts::OS, env::consts::ARCH)
}

/// Select a single native plugin descriptor for the current platform from `kovrek.json` / index metadata.
///
/// Accepts either legacy flat objects (`{"library": "...", "source": "..."}`)
/// or per-platform tables (`{"windows-x86_64": {...}, "linux-x86_64": {...}}`).
pub(crate) fn select_native_spec(native_val: &JsonValue) -> Option<JsonValue> {
    let obj = native_val.as_object()?;
    if obj.contains_key("library")
        || obj.contains_key("source")
        || obj.contains_key("url")
        || obj.contains_key("artifact")
        || obj.contains_key("crate_type")
    {
        return Some(native_val.clone());
    }
    let table = obj
        .get("targets")
        .and_then(JsonValue::as_object)
        .unwrap_or(obj);
    let full = current_platform_key();
    table
        .get(&full)
        .or_else(|| table.get(env::consts::OS))
        .or_else(|| table.get("default"))
        .filter(|v| v.is_object())
        .cloned()
}

/// Find the nearest ancestor `kovrek.json` directory that declares a `"workspaces"` array.
pub(crate) fn workspace_root(start: &Path) -> Option<PathBuf> {
    let mut cur = if start.is_file() {
        start.parent()?.to_path_buf()
    } else {
        start.to_path_buf()
    };
    loop {
        let manifest_path = cur.join("kovrek.json");
        if manifest_path.is_file() {
            if let Ok(txt) = fs::read_to_string(&manifest_path) {
                if let Ok(json) = serde_json::from_str::<JsonValue>(&txt) {
                    if json
                        .get("workspaces")
                        .and_then(JsonValue::as_array)
                        .is_some()
                    {
                        return Some(cur);
                    }
                }
            }
        }
        if !cur.pop() {
            break;
        }
    }
    None
}

/// Expand workspace globs (`packages/*` or explicit directories) under `root`.
pub(crate) fn resolve_workspace_members(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(txt) = fs::read_to_string(root.join("kovrek.json")) else {
        return out;
    };
    let Ok(manifest) = serde_json::from_str::<JsonValue>(&txt) else {
        return out;
    };
    let Some(patterns) = manifest.get("workspaces").and_then(JsonValue::as_array) else {
        return out;
    };
    let mut seen = HashSet::new();
    for pat in patterns {
        let Some(raw) = pat.as_str() else {
            continue;
        };
        let norm = raw.trim().replace('\\', "/");
        if let Some(prefix) = norm.strip_suffix("/*") {
            let Ok(parent_rel) = safe_package_relative_path(prefix) else {
                continue;
            };
            let parent = root.join(parent_rel);
            if let Ok(entries) = fs::read_dir(parent) {
                for ent in entries.flatten() {
                    let p = ent.path();
                    if p.is_dir() && p.join("kovrek.json").is_file() && seen.insert(p.clone()) {
                        out.push(p);
                    } else if p.is_dir()
                        && p.file_name()
                            .and_then(|n| n.to_str())
                            .map(|s| s.starts_with('@'))
                            .unwrap_or(false)
                    {
                        if let Ok(scoped) = fs::read_dir(&p) {
                            for sub in scoped.flatten() {
                                let sp = sub.path();
                                if sp.is_dir()
                                    && sp.join("kovrek.json").is_file()
                                    && seen.insert(sp.clone())
                                {
                                    out.push(sp);
                                }
                            }
                        }
                    }
                }
            }
        } else if let Ok(rel) = safe_package_relative_path(&norm) {
            let dir = root.join(rel);
            if dir.is_dir() && dir.join("kovrek.json").is_file() && seen.insert(dir.clone()) {
                out.push(dir);
            }
        }
    }
    out
}

/// Locate a workspace member whose `kovrek.json` `"name"` matches `name`.
pub(crate) fn find_workspace_package(
    project_or_file: &Path,
    name: &str,
) -> Option<(PathBuf, JsonValue)> {
    let mut roots = Vec::new();
    roots.push(project_or_file.to_path_buf());
    if let Some(ws_root) = workspace_root(project_or_file) {
        if ws_root != project_or_file {
            roots.push(ws_root);
        }
    }
    for ws in roots {
        for member_dir in resolve_workspace_members(&ws) {
            if let Ok(txt) = fs::read_to_string(member_dir.join("kovrek.json")) {
                if let Ok(m) = serde_json::from_str::<JsonValue>(&txt) {
                    if m.get("name").and_then(JsonValue::as_str) == Some(name) {
                        return Some((member_dir, m));
                    }
                }
            }
        }
    }
    None
}

fn copy_package_tree(src: &Path, dest: &Path) -> Result<(), String> {
    fs::create_dir_all(dest).map_err(|e| e.to_string())?;
    for entry in fs::read_dir(src).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if matches!(
            name_str.as_ref(),
            "vendor" | "target" | ".git" | "kovrek.lock"
        ) {
            continue;
        }
        let from = entry.path();
        let to = dest.join(name);
        if from.is_dir() {
            copy_package_tree(&from, &to)?;
        } else {
            fs::copy(&from, &to).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

pub(crate) fn write_package_lock(root: &Path, manifest: &JsonValue) -> Result<(), String> {
    let mut locked = serde_json::Map::new();
    if let Some(packages) = manifest.get("packages").and_then(JsonValue::as_object) {
        for (name, meta) in packages {
            let mut entry = if meta.is_object() {
                meta.clone()
            } else {
                serde_json::json!({ "version": meta.as_str().unwrap_or("0.0.0") })
            };
            let vdir = vendor_pkg_dir(root, name);
            let pkg_dir = if vdir.exists() {
                vdir
            } else if let Some((ws_dir, _)) = find_workspace_package(root, name) {
                entry["workspace"] = JsonValue::Bool(true);
                ws_dir
            } else {
                vdir
            };
            let hash = hash_directory(&pkg_dir)?;
            entry["sha256"] = JsonValue::String(hash);
            if pkg_dir.join("kovrek.sig.json").is_file() {
                entry["signed"] = JsonValue::Bool(true);
            }
            if let Ok(m_txt) = fs::read_to_string(pkg_dir.join("kovrek.json")) {
                if let Ok(m_json) = serde_json::from_str::<JsonValue>(&m_txt) {
                    if m_json.get("native").is_some() {
                        entry["native"] = JsonValue::Bool(true);
                    }
                    if let Some(deps) = m_json.get("dependencies") {
                        entry["dependencies"] = deps.clone();
                    }
                }
            }
            locked.insert(name.clone(), entry);
        }
    }
    let lock = serde_json::json!({
        "format": 3,
        "engine": VERSION,
        "packages": JsonValue::Object(locked)
    });
    fs::write(
        root.join("kovrek.lock"),
        serde_json::to_string_pretty(&lock).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())
}

fn install_platform_native_artifact(
    pkg: &JsonValue,
    pkg_base: &str,
    dest: &Path,
) -> Result<(), String> {
    let Some(native_val) = pkg.get("native") else {
        return Ok(());
    };
    let Some(nat) = select_native_spec(native_val) else {
        return Ok(());
    };
    let Some(raw_url) = nat
        .get("url")
        .or_else(|| nat.get("artifact"))
        .and_then(JsonValue::as_str)
    else {
        return Ok(());
    };
    let raw_lib = nat
        .get("library")
        .and_then(JsonValue::as_str)
        .unwrap_or("plugin");
    let mut lib_rel = safe_package_relative_path(raw_lib)?;
    if lib_rel.extension().is_none() {
        lib_rel.set_extension(crate::plugin::platform_shared_ext());
    }
    let target = dest.join(&lib_rel);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let resolved = resolve_pkg_ref(pkg_base, raw_url);
    let bytes = fetch_package_file(&resolved)?;
    if let Some(expected_sha) = nat.get("sha256").and_then(JsonValue::as_str) {
        let mut h = Sha256::new();
        h.update(&bytes);
        let actual = format!("{:x}", h.finalize());
        if actual != expected_sha {
            return Err(format!(
                "native artifact sha256 mismatch for '{}': expected {expected_sha}, got {actual}",
                target.display()
            ));
        }
    }
    fs::write(&target, bytes).map_err(|e| e.to_string())?;
    let pkg_manifest_path = dest.join("kovrek.json");
    if !pkg_manifest_path.is_file() {
        let synth = serde_json::json!({
            "name": pkg.get("name").and_then(JsonValue::as_str).unwrap_or("native"),
            "version": pkg.get("version").and_then(JsonValue::as_str).unwrap_or("0.1.0"),
            "entry": pkg.get("entry").and_then(JsonValue::as_str).unwrap_or("main.kvr"),
            "native": native_val
        });
        fs::write(
            &pkg_manifest_path,
            serde_json::to_string_pretty(&synth).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub(crate) fn pkg_add_inner_at(
    root: &Path,
    spec: &str,
    seen: &mut HashSet<String>,
) -> Result<(), String> {
    let (name, constraint) = parse_pkg_spec(spec)?;
    validate_package_name(name)?;
    if !seen.insert(name.to_string()) {
        return Ok(());
    }
    let dest = vendor_pkg_dir(root, name);

    let (version, entry, from_workspace) = if let Some((ws_dir, ws_manifest)) =
        find_workspace_package(root, name)
    {
        let version = ws_manifest
            .get("version")
            .and_then(JsonValue::as_str)
            .unwrap_or("0.1.0")
            .to_string();
        if let Some(req) = constraint {
            if !semver_matches(&version, req) {
                return Err(format!(
                    "workspace package '{name}' version {version} does not match constraint '{req}'"
                ));
            }
        }
        let entry = ws_manifest
            .get("entry")
            .and_then(JsonValue::as_str)
            .unwrap_or("src/main.kvr")
            .to_string();
        if let Some(deps) = ws_manifest
            .get("dependencies")
            .and_then(JsonValue::as_array)
        {
            for dep in deps {
                let dep_spec = dep
                    .as_str()
                    .ok_or("package dependency names must be Strings")?;
                pkg_add_inner_at(root, dep_spec, seen)?;
            }
        }
        if let Some(pkgs) = ws_manifest.get("packages").and_then(JsonValue::as_object) {
            for (dep_name, dep_meta) in pkgs {
                let spec_str = match dep_meta.get("version").and_then(JsonValue::as_str) {
                    Some(v) => format!("{dep_name}@{v}"),
                    None => match dep_meta.as_str() {
                        Some(v) => format!("{dep_name}@{v}"),
                        None => dep_name.clone(),
                    },
                };
                pkg_add_inner_at(root, &spec_str, seen)?;
            }
        }
        if dest.exists() {
            fs::remove_dir_all(&dest).map_err(|e| e.to_string())?;
        }
        copy_package_tree(&ws_dir, &dest)?;
        (version, entry, true)
    } else {
        let (index_path, index) = pkg_index()?;
        let pkg = index
            .get("packages")
            .and_then(|x| x.get(name))
            .ok_or(format!("package '{name}' not found in Kovrek registry"))?;
        let version = pkg
            .get("version")
            .and_then(JsonValue::as_str)
            .unwrap_or("0.0.0")
            .to_string();
        if let Some(req) = constraint {
            if !semver_matches(&version, req) {
                return Err(format!(
                    "package '{name}' version {version} does not match requested constraint '{req}'"
                ));
            }
        }
        let entry = pkg
            .get("entry")
            .and_then(JsonValue::as_str)
            .unwrap_or("main.kvr")
            .to_string();
        if let Some(dependencies) = pkg.get("dependencies").and_then(JsonValue::as_array) {
            for dependency in dependencies {
                let dependency = dependency
                    .as_str()
                    .ok_or("package dependency names must be Strings")?;
                pkg_add_inner_at(root, dependency, seen)?;
            }
        }
        let pkg_base = pkg
            .get("_registry_base")
            .and_then(JsonValue::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| {
                index_path
                    .parent()
                    .unwrap_or(Path::new("."))
                    .to_string_lossy()
                    .into_owned()
            });
        let pkg_index_parent = pkg
            .get("_registry_index")
            .and_then(JsonValue::as_str)
            .and_then(|p| Path::new(p).parent().map(Path::to_path_buf))
            .unwrap_or_else(|| index_path.parent().unwrap_or(Path::new(".")).to_path_buf());

        if dest.exists() {
            fs::remove_dir_all(&dest).map_err(|e| e.to_string())?;
        }

        if let Some(files) = pkg.get("files").and_then(JsonValue::as_object) {
            fs::create_dir_all(&dest).map_err(|e| e.to_string())?;
            for (relative, url) in files {
                let relative = safe_package_relative_path(relative)?;
                let url = url
                    .as_str()
                    .ok_or("registry package file URL must be String")?;
                let target = dest.join(relative);
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                let resolved = resolve_pkg_ref(&pkg_base, url);
                fs::write(&target, fetch_package_file(&resolved)?).map_err(|e| e.to_string())?;
            }
        } else if let Some(bundle_rel) = pkg.get("bundle_path").and_then(JsonValue::as_str) {
            let bundle_url = resolve_pkg_ref(&pkg_base, bundle_rel);
            let bytes = fetch_package_file(&bundle_url)?;
            let bundle: JsonValue = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
            unpack_bundle_to_dir(&bundle, &dest)?;
        } else if let Some(url) = pkg.get("url").and_then(JsonValue::as_str) {
            fs::create_dir_all(&dest).map_err(|e| e.to_string())?;
            let relative = safe_package_relative_path(&entry)?;
            let target = dest.join(relative);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            let resolved = resolve_pkg_ref(&pkg_base, url);
            fs::write(&target, fetch_package_file(&resolved)?).map_err(|e| e.to_string())?;
        } else {
            let source = pkg
                .get("source")
                .and_then(JsonValue::as_str)
                .ok_or("registry package requires source, url, or files")?;
            let source_path = pkg_index_parent.join(source);
            copy_dir_all(&source_path, &dest)?;
        }

        if let Some(expected_sha) = pkg.get("sha256").and_then(JsonValue::as_str) {
            let content_sha = hash_directory_excluding_sig(&dest)?;
            let full_sha = hash_directory(&dest)?;
            if content_sha != expected_sha && full_sha != expected_sha {
                let _ = fs::remove_dir_all(&dest);
                return Err(format!(
                    "package '{name}' failed registry sha256 verification (expected {expected_sha}, got {content_sha})"
                ));
            }
        }
        install_platform_native_artifact(pkg, &pkg_base, &dest)?;
        if dest.join("kovrek.sig.json").is_file() {
            let _ = verify_package_dir_sig(&dest)?;
        }
        (version, entry, false)
    };

    let manifest_path = root.join("kovrek.json");
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
    let mut pkg_meta = serde_json::json!({"version": version, "entry": entry});
    if from_workspace {
        pkg_meta["workspace"] = JsonValue::Bool(true);
    }
    manifest["packages"][name] = pkg_meta;
    fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    write_package_lock(root, &manifest)?;
    println!("Added {name} {version} -> vendor/{name}/{entry}");
    Ok(())
}

pub(crate) fn pkg_add_inner(spec: &str, seen: &mut HashSet<String>) -> Result<(), String> {
    let root = project_root(&env::current_dir().map_err(|e| e.to_string())?)?;
    pkg_add_inner_at(&root, spec, seen)
}

pub(crate) fn pkg_add(name: &str) -> Result<(), String> {
    pkg_add_inner(name, &mut HashSet::new())
}

pub(crate) fn pkg_remove(name: &str) -> Result<(), String> {
    validate_package_name(name)?;
    let root = project_root(&env::current_dir().map_err(|e| e.to_string())?)?;
    let dest = vendor_pkg_dir(&root, name);
    if dest.exists() {
        fs::remove_dir_all(&dest).map_err(|e| e.to_string())?;
        if name.starts_with('@') {
            if let Some(scope_dir) = dest.parent() {
                if fs::read_dir(scope_dir)
                    .map(|mut it| it.next().is_none())
                    .unwrap_or(false)
                {
                    let _ = fs::remove_dir(scope_dir);
                }
            }
        }
    }
    let manifest_path = root.join("kovrek.json");
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

pub(crate) fn pkg_list() -> Result<(), String> {
    let (_, index) = pkg_index()?;
    println!("Kovrek Registry");
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

fn install_project_packages(project_dir: &Path) -> Result<(), String> {
    let manifest: JsonValue = serde_json::from_str(
        &fs::read_to_string(project_dir.join("kovrek.json")).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let mut seen = HashSet::new();
    if let Some(pkgs) = manifest.get("packages").and_then(JsonValue::as_object) {
        let specs: Vec<String> = pkgs
            .iter()
            .map(|(name, meta)| match meta.as_str() {
                Some(ver) => format!("{name}@{ver}"),
                None => name.clone(),
            })
            .collect();
        for spec in specs {
            pkg_add_inner_at(project_dir, &spec, &mut seen)?;
        }
    }
    Ok(())
}

pub(crate) fn pkg_install() -> Result<(), String> {
    let root = project_root(&env::current_dir().map_err(|e| e.to_string())?)?;
    install_project_packages(&root)?;
    for member_dir in resolve_workspace_members(&root) {
        install_project_packages(&member_dir)?;
    }
    println!("Kovrek packages restored");
    Ok(())
}

pub(crate) fn pkg_search(term: &str) -> Result<(), String> {
    let (_, index) = pkg_index()?;
    println!("Kovrek Registry search: {term}");
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

pub(crate) fn pkg_publish() -> Result<(), String> {
    let root = project_root(&env::current_dir().map_err(|e| e.to_string())?)?;
    let manifest_path = root.join("kovrek.json");
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
        .unwrap_or("src/main.kvr");

    let home = kovrek_home();
    let pkg_root = home.join("packages").join(name);
    if pkg_root.exists() {
        fs::remove_dir_all(&pkg_root).map_err(|e| e.to_string())?;
    }
    fs::create_dir_all(&pkg_root).map_err(|e| e.to_string())?;
    if root.join("src").is_dir() {
        copy_dir_all(&root.join("src"), &pkg_root.join("src"))?;
    }
    let _ = fs::copy(&manifest_path, pkg_root.join("kovrek.json"));
    if root.join("kovrek.sig.json").is_file() {
        let _ = fs::copy(
            root.join("kovrek.sig.json"),
            pkg_root.join("kovrek.sig.json"),
        );
    }
    let root_entry = root.join(entry);
    if root_entry.is_file() && !root_entry.starts_with(root.join("src")) {
        if let Ok(rel) = root_entry.strip_prefix(&root) {
            let target = pkg_root.join(rel);
            if let Some(parent) = target.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let _ = fs::copy(&root_entry, &target);
        }
    }
    let pkg_sha = hash_directory_excluding_sig(&pkg_root)?;

    if let Ok(remote_url) = env::var("KOVREK_REGISTRY_URL") {
        if remote_url.starts_with("http://") || remote_url.starts_with("https://") {
            #[cfg(feature = "native-services")]
            {
                let bundle = build_package_bundle(&pkg_root)?;
                let publish_url = if let Some(base) = remote_url.strip_suffix("/index.json") {
                    format!("{base}/publish")
                } else if let Some(base) = remote_url.strip_suffix("/registry/index.json") {
                    format!("{base}/publish")
                } else {
                    format!("{}/publish", remote_url.trim_end_matches('/'))
                };
                let client = reqwest::blocking::Client::new();
                let payload = serde_json::to_string(&bundle).map_err(|e| e.to_string())?;
                let resp = client
                    .post(&publish_url)
                    .header("Content-Type", "application/json")
                    .body(payload)
                    .send()
                    .map_err(|e| e.to_string())?;
                if !resp.status().is_success() {
                    return Err(format!(
                        "remote registry publish returned HTTP {}",
                        resp.status()
                    ));
                }
                println!("Published {name} {version} to {publish_url}");
                return Ok(());
            }
        }
    }

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
    let mut meta_obj = serde_json::json!({
        "version": version,
        "entry": entry,
        "source": format!("../packages/{name}"),
        "sha256": pkg_sha
    });
    if let Some(deps) = manifest.get("dependencies") {
        meta_obj["dependencies"] = deps.clone();
    }
    if let Some(nat) = manifest.get("native") {
        meta_obj["native"] = nat.clone();
    }
    index["packages"][name] = meta_obj;
    fs::write(
        &index_path,
        serde_json::to_string_pretty(&index).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    println!("Published {name} {version} to {}", index_path.display());
    Ok(())
}

pub(crate) fn pkg_audit() -> Result<(), String> {
    let root = project_root(&env::current_dir().map_err(|e| e.to_string())?)?;
    let lock_path = root.join("kovrek.lock");
    if !lock_path.exists() {
        return Err("kovrek.lock not found; run kovrek install/add first".into());
    }
    let lock: JsonValue =
        serde_json::from_str(&fs::read_to_string(&lock_path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let packages = lock
        .get("packages")
        .and_then(JsonValue::as_object)
        .ok_or("invalid lockfile packages")?;
    let mut checked = 0usize;
    let mut signed = 0usize;
    let mut caps_summary = Vec::new();
    if let Ok(manifest_txt) = fs::read_to_string(root.join("kovrek.json"))
        && let Ok(manifest_json) = serde_json::from_str::<JsonValue>(&manifest_txt)
        && let Some(caps) = manifest_json
            .get("capabilities")
            .and_then(JsonValue::as_array)
    {
        let names: Vec<String> = caps
            .iter()
            .filter_map(JsonValue::as_str)
            .map(ToString::to_string)
            .collect();
        if !names.is_empty() {
            caps_summary.push(format!("root=[{}]", names.join(",")));
        }
    }
    for (name, meta) in packages {
        let expected = meta
            .get("sha256")
            .and_then(JsonValue::as_str)
            .ok_or(format!("package {name} has no sha256"))?;
        let pkg_dir = vendor_pkg_dir(&root, name);
        let actual = hash_directory(&pkg_dir)?;
        if actual != expected {
            return Err(format!("package integrity mismatch: {name}"));
        }
        if let Ok(pkg_txt) = fs::read_to_string(pkg_dir.join("kovrek.json"))
            && let Ok(pkg_json) = serde_json::from_str::<JsonValue>(&pkg_txt)
            && let Some(caps) = pkg_json.get("capabilities").and_then(JsonValue::as_array)
        {
            let names: Vec<String> = caps
                .iter()
                .filter_map(JsonValue::as_str)
                .map(ToString::to_string)
                .collect();
            if !names.is_empty() {
                caps_summary.push(format!("{name}=[{}]", names.join(",")));
            }
        }
        if pkg_dir.join("kovrek.sig.json").is_file() {
            if verify_package_dir_sig(&pkg_dir)? {
                signed += 1;
            }
        }
        checked += 1;
    }
    let cap_suffix = if caps_summary.is_empty() {
        String::new()
    } else {
        format!("; capabilities: {}", caps_summary.join(" "))
    };
    if signed > 0 {
        println!(
            "Audit OK: {checked} package(s), content hashes verified ({signed} cryptographically signed){cap_suffix}"
        );
    } else {
        println!("Audit OK: {checked} package(s), content hashes verified{cap_suffix}");
    }
    Ok(())
}

fn keys_dir() -> PathBuf {
    kovrek_home().join("keys")
}

fn find_signing_key(key_name: Option<&str>) -> Result<(String, String), String> {
    if let Ok(secret) = env::var("KOVREK_SIGNING_KEY") {
        let mut h = Sha256::new();
        h.update(secret.as_bytes());
        let fp = format!("{:x}", h.finalize());
        return Ok((fp[..16].to_string(), secret));
    }
    let dir = keys_dir();
    if let Some(name) = key_name {
        let path = dir.join(format!("{name}.json"));
        let txt = fs::read_to_string(&path)
            .map_err(|e| format!("signing key '{name}' not found at {}: {e}", path.display()))?;
        let json: JsonValue = serde_json::from_str(&txt).map_err(|e| e.to_string())?;
        let key_id = json
            .get("key_id")
            .and_then(JsonValue::as_str)
            .ok_or("keyfile missing key_id")?
            .to_string();
        let secret = json
            .get("secret")
            .and_then(JsonValue::as_str)
            .ok_or("keyfile missing secret")?
            .to_string();
        return Ok((key_id, secret));
    }
    let default_path = dir.join("default.json");
    if default_path.is_file() {
        return find_signing_key(Some("default"));
    }
    if let Ok(entries) = fs::read_dir(&dir) {
        for ent in entries.flatten() {
            let p = ent.path();
            if p.extension().and_then(|e| e.to_str()) == Some("json") {
                if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                    return find_signing_key(Some(stem));
                }
            }
        }
    }
    Err("no Kovrek signing key found; run 'kovrek pkg keygen [name]' first".into())
}

fn find_trusted_secret_by_id(key_id: &str) -> Option<String> {
    if let Ok(secret) = env::var("KOVREK_SIGNING_KEY") {
        let mut h = Sha256::new();
        h.update(secret.as_bytes());
        let fp = format!("{:x}", h.finalize());
        if fp.starts_with(key_id) {
            return Some(secret);
        }
    }
    let dir = keys_dir();
    let Ok(entries) = fs::read_dir(dir) else {
        return None;
    };
    for ent in entries.flatten() {
        let p = ent.path();
        if p.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(txt) = fs::read_to_string(&p) else {
            continue;
        };
        let Ok(json) = serde_json::from_str::<JsonValue>(&txt) else {
            continue;
        };
        if json.get("key_id").and_then(JsonValue::as_str) == Some(key_id) {
            if let Some(secret) = json.get("secret").and_then(JsonValue::as_str) {
                return Some(secret.to_string());
            }
        }
    }
    None
}

fn hash_directory_excluding_sig(dir: &Path) -> Result<String, String> {
    let mut files = Vec::new();
    collect_files_excluding(dir, &mut files)?;
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

fn collect_files_excluding(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    for ent in fs::read_dir(dir).map_err(|e| e.to_string())? {
        let p = ent.map_err(|e| e.to_string())?.path();
        if p.file_name().and_then(|s| s.to_str()) == Some("kovrek.sig.json") {
            continue;
        }
        if p.is_dir() {
            collect_files_excluding(&p, out)?;
        } else {
            out.push(p);
        }
    }
    Ok(())
}

fn verify_package_dir_sig(dir: &Path) -> Result<bool, String> {
    let sig_path = dir.join("kovrek.sig.json");
    if !sig_path.is_file() {
        return Ok(false);
    }
    let sig: JsonValue =
        serde_json::from_str(&fs::read_to_string(&sig_path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let expected_sha = sig
        .get("sha256")
        .and_then(JsonValue::as_str)
        .ok_or("kovrek.sig.json missing sha256")?;
    let actual_sha = hash_directory_excluding_sig(dir)?;
    if actual_sha != expected_sha {
        return Err(format!(
            "package signature hash mismatch in {}: expected {expected_sha}, got {actual_sha}",
            dir.display()
        ));
    }
    let key_id = sig
        .get("key_id")
        .and_then(JsonValue::as_str)
        .ok_or("kovrek.sig.json missing key_id")?;
    let signature = sig
        .get("signature")
        .and_then(JsonValue::as_str)
        .ok_or("kovrek.sig.json missing signature")?;
    if let Some(secret) = find_trusted_secret_by_id(key_id) {
        let expected_sig = hmac_sha256_hex(&secret, &format!("kovrek-pkg-v1:{expected_sha}"));
        if !constant_time_strings_equal(&expected_sig, signature) {
            return Err(format!(
                "invalid HMAC signature in {} (key_id={key_id})",
                dir.display()
            ));
        }
    }
    Ok(true)
}

pub(crate) fn pkg_keygen(name: Option<&str>) -> Result<(), String> {
    let key_name = name.unwrap_or("default");
    let mut raw = [0u8; 32];
    rand::thread_rng().fill(&mut raw);
    let secret: String = raw.iter().map(|b| format!("{b:02x}")).collect();
    let mut h = Sha256::new();
    h.update(secret.as_bytes());
    let fp = format!("{:x}", h.finalize());
    let key_id = fp[..16].to_string();
    let dir = keys_dir();
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(format!("{key_name}.json"));
    let obj = serde_json::json!({
        "format": 1,
        "name": key_name,
        "key_id": key_id,
        "algorithm": "hmac-sha256",
        "secret": secret,
        "created_at": chrono::Utc::now().to_rfc3339()
    });
    fs::write(
        &path,
        serde_json::to_string_pretty(&obj).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    println!(
        "Generated Kovrek signing key '{key_name}' (key_id={key_id}) -> {}",
        path.display()
    );
    Ok(())
}

pub(crate) fn pkg_sign(dir: Option<&Path>, key_name: Option<&str>) -> Result<(), String> {
    let target = match dir {
        Some(d) => d.to_path_buf(),
        None => project_root(&env::current_dir().map_err(|e| e.to_string())?)?,
    };
    let (key_id, secret) = find_signing_key(key_name)?;
    let sha = hash_directory_excluding_sig(&target)?;
    let signature = hmac_sha256_hex(&secret, &format!("kovrek-pkg-v1:{sha}"));
    let (pkg_name, version) = fs::read_to_string(target.join("kovrek.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<JsonValue>(&s).ok())
        .map(|m| {
            (
                m.get("name")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("package")
                    .to_string(),
                m.get("version")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("0.0.0")
                    .to_string(),
            )
        })
        .unwrap_or_else(|| ("package".into(), "0.0.0".into()));
    let sig_obj = serde_json::json!({
        "format": 1,
        "algorithm": "hmac-sha256",
        "package": pkg_name,
        "version": version,
        "key_id": key_id,
        "sha256": sha,
        "signature": signature,
        "signed_at": chrono::Utc::now().to_rfc3339()
    });
    let sig_path = target.join("kovrek.sig.json");
    fs::write(
        &sig_path,
        serde_json::to_string_pretty(&sig_obj).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    println!(
        "Signed {pkg_name} {version} (key_id={key_id}) -> {}",
        sig_path.display()
    );
    Ok(())
}

pub(crate) fn pkg_verify(target: Option<&str>) -> Result<(), String> {
    if let Some(t) = target {
        let p = PathBuf::from(t);
        if p.is_file() {
            let bundle: JsonValue =
                serde_json::from_str(&fs::read_to_string(&p).map_err(|e| e.to_string())?)
                    .map_err(|e| e.to_string())?;
            let name = bundle
                .get("name")
                .and_then(JsonValue::as_str)
                .unwrap_or("bundle");
            verify_bundle_json(&bundle)?;
            println!("Verified package bundle '{name}'");
            return Ok(());
        }
        if p.is_dir() {
            let _ = verify_package_dir_sig(&p)?;
            println!("Verified package directory {}", p.display());
            return Ok(());
        }
        validate_package_name(t)?;
        let root = project_root(&env::current_dir().map_err(|e| e.to_string())?)?;
        let pkg_dir = vendor_pkg_dir(&root, t);
        if !pkg_dir.is_dir() {
            return Err(format!("installed package or path '{t}' not found"));
        }
        let _ = verify_package_dir_sig(&pkg_dir)?;
        println!("Verified installed package '{t}'");
        return Ok(());
    }
    pkg_audit()
}

pub(crate) fn pkg_info(name: &str) -> Result<(), String> {
    let (_, index) = pkg_index()?;
    let meta = index
        .get("packages")
        .and_then(|x| x.get(name))
        .ok_or_else(|| format!("package '{name}' not found in Kovrek registry"))?;
    let version = meta
        .get("version")
        .and_then(JsonValue::as_str)
        .unwrap_or("?");
    let entry = meta.get("entry").and_then(JsonValue::as_str).unwrap_or("?");
    let source = meta
        .get("source")
        .or_else(|| meta.get("url"))
        .and_then(JsonValue::as_str)
        .unwrap_or("(multi-file)");
    println!("Package : {name}");
    println!("Version : {version}");
    println!("Entry   : {entry}");
    println!("Source  : {source}");
    if let Some(sha) = meta.get("sha256").and_then(JsonValue::as_str) {
        println!("SHA-256 : {sha}");
    }
    if let Some(deps) = meta.get("dependencies").and_then(JsonValue::as_array) {
        let names: Vec<&str> = deps.iter().filter_map(JsonValue::as_str).collect();
        println!(
            "Deps    : {}",
            if names.is_empty() {
                "none".into()
            } else {
                names.join(", ")
            }
        );
    } else {
        println!("Deps    : none");
    }
    println!(
        "Native  : {}",
        if meta.get("native").is_some() {
            "yes (kovrek_plugin_v1)"
        } else {
            "no (pure Kovrek / VM)"
        }
    );
    Ok(())
}

pub(crate) fn pkg_tree() -> Result<(), String> {
    let root = project_root(&env::current_dir().map_err(|e| e.to_string())?)?;
    let manifest: JsonValue = serde_json::from_str(
        &fs::read_to_string(root.join("kovrek.json")).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let proj_name = manifest
        .get("name")
        .and_then(JsonValue::as_str)
        .unwrap_or("kovrek-app");
    let proj_ver = manifest
        .get("version")
        .and_then(JsonValue::as_str)
        .unwrap_or("0.1.0");
    println!("{proj_name}@{proj_ver}");
    let pkgs = manifest
        .get("packages")
        .and_then(JsonValue::as_object)
        .cloned()
        .unwrap_or_default();
    let (_, index) = pkg_index().unwrap_or_else(|_| (PathBuf::new(), serde_json::json!({})));
    fn print_node(
        name: &str,
        root: &Path,
        index: &JsonValue,
        prefix: &str,
        last: bool,
        seen: &mut HashSet<String>,
    ) {
        let branch = if last { "`-- " } else { "|-- " };
        let pkg_dir = vendor_pkg_dir(root, name);
        let ver = index
            .get("packages")
            .and_then(|p| p.get(name))
            .and_then(|m| m.get("version"))
            .and_then(JsonValue::as_str)
            .unwrap_or("?");
        let mut tags = Vec::new();
        if pkg_dir.join("kovrek.sig.json").is_file() {
            tags.push("signed");
        }
        if let Ok(t) = fs::read_to_string(pkg_dir.join("kovrek.json")) {
            if serde_json::from_str::<JsonValue>(&t)
                .ok()
                .and_then(|j| j.get("native").cloned())
                .is_some()
            {
                tags.push("native");
            }
        }
        let suffix = if tags.is_empty() {
            String::new()
        } else {
            format!(" [{}]", tags.join(", "))
        };
        println!("{prefix}{branch}{name}@{ver}{suffix}");
        if !seen.insert(name.to_string()) {
            return;
        }
        let deps: Vec<String> = index
            .get("packages")
            .and_then(|p| p.get(name))
            .and_then(|m| m.get("dependencies"))
            .and_then(JsonValue::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(JsonValue::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        let next_prefix = format!("{prefix}{}", if last { "    " } else { "|   " });
        for (idx, dep) in deps.iter().enumerate() {
            print_node(dep, root, index, &next_prefix, idx + 1 == deps.len(), seen);
        }
    }
    let total = pkgs.len();
    for (idx, name) in pkgs.keys().enumerate() {
        print_node(
            name,
            &root,
            &index,
            "",
            idx + 1 == total,
            &mut HashSet::new(),
        );
    }
    Ok(())
}

pub(crate) fn pkg_update(target: Option<&str>) -> Result<(), String> {
    let root = project_root(&env::current_dir().map_err(|e| e.to_string())?)?;
    let manifest: JsonValue = serde_json::from_str(
        &fs::read_to_string(root.join("kovrek.json")).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let names: Vec<String> = if let Some(one) = target {
        vec![one.to_string()]
    } else {
        manifest
            .get("packages")
            .and_then(JsonValue::as_object)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    };
    let mut seen = HashSet::new();
    for name in &names {
        pkg_add_inner(name, &mut seen)?;
    }
    println!("Updated {} package(s)", names.len());
    Ok(())
}

pub(crate) fn build_package_bundle(dir: &Path) -> Result<JsonValue, String> {
    let manifest: JsonValue = serde_json::from_str(
        &fs::read_to_string(dir.join("kovrek.json")).unwrap_or_else(|_| {
            serde_json::json!({"name":"package","version":"0.1.0","entry":"src/main.kvr"})
                .to_string()
        }),
    )
    .map_err(|e| e.to_string())?;
    let name = manifest
        .get("name")
        .and_then(JsonValue::as_str)
        .unwrap_or("package");
    let version = manifest
        .get("version")
        .and_then(JsonValue::as_str)
        .unwrap_or("0.1.0");
    let entry = manifest
        .get("entry")
        .and_then(JsonValue::as_str)
        .unwrap_or("src/main.kvr");
    let mut files = Vec::new();
    collect_files_excluding(dir, &mut files)?;
    files.sort();
    let mut encoded = BTreeMap::new();
    for p in files {
        let rel = p
            .strip_prefix(dir)
            .unwrap_or(&p)
            .to_string_lossy()
            .replace('\\', "/");
        if rel == "kovrek.lock" || rel.ends_with(".kvpkg.json") {
            continue;
        }
        let bytes = fs::read(&p).map_err(|e| e.to_string())?;
        encoded.insert(
            rel,
            JsonValue::String(base64::engine::general_purpose::STANDARD.encode(bytes)),
        );
    }
    let sha256 = hash_directory_excluding_sig(dir)?;
    let sig = if dir.join("kovrek.sig.json").is_file() {
        serde_json::from_str::<JsonValue>(
            &fs::read_to_string(dir.join("kovrek.sig.json")).map_err(|e| e.to_string())?,
        )
        .ok()
    } else {
        None
    };
    let normalized_caps: Vec<String> = manifest
        .get("capabilities")
        .and_then(JsonValue::as_array)
        .map(|arr| {
            let set: std::collections::BTreeSet<String> = arr
                .iter()
                .filter_map(JsonValue::as_str)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            set.into_iter().collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(serde_json::json!({
        "format": "kovrek.bundle.v1",
        "name": name,
        "version": version,
        "entry": entry,
        "capabilities": normalized_caps,
        "dependencies": manifest.get("dependencies").cloned().unwrap_or_else(|| serde_json::json!([])),
        "native": manifest.get("native").cloned().unwrap_or(JsonValue::Null),
        "sha256": sha256,
        "signature": sig,
        "files": encoded
    }))
}

fn verify_bundle_json(bundle: &JsonValue) -> Result<(), String> {
    if bundle.get("format").and_then(JsonValue::as_str) != Some("kovrek.bundle.v1") {
        return Err("unsupported .kvpkg bundle format".into());
    }
    let expected = bundle
        .get("sha256")
        .and_then(JsonValue::as_str)
        .ok_or("bundle missing sha256")?;
    let files = bundle
        .get("files")
        .and_then(JsonValue::as_object)
        .ok_or("bundle missing files map")?;
    let mut ordered = BTreeMap::new();
    for (rel, b64) in files {
        let safe = safe_package_relative_path(rel)?;
        let raw = base64::engine::general_purpose::STANDARD
            .decode(
                b64.as_str()
                    .ok_or("bundle file payload must be base64 string")?,
            )
            .map_err(|e| e.to_string())?;
        ordered.insert(safe.to_string_lossy().replace('\\', "/"), raw);
    }
    let mut h = Sha256::new();
    for (rel, bytes) in ordered {
        h.update(rel.as_bytes());
        h.update([0]);
        h.update(bytes);
        h.update([0]);
    }
    let actual = format!("{:x}", h.finalize());
    if actual != expected {
        return Err(format!(
            "bundle integrity mismatch: expected {expected}, got {actual}"
        ));
    }
    Ok(())
}

pub(crate) fn pkg_pack(dir: Option<&Path>, out: Option<&Path>) -> Result<(), String> {
    let target = match dir {
        Some(d) => d.to_path_buf(),
        None => project_root(&env::current_dir().map_err(|e| e.to_string())?)?,
    };
    let bundle = build_package_bundle(&target)?;
    let name = bundle["name"].as_str().unwrap_or("package");
    let ver = bundle["version"].as_str().unwrap_or("0.1.0");
    let out_path = out
        .map(Path::to_path_buf)
        .unwrap_or_else(|| target.join(format!("{name}-{ver}.kvpkg.json")));
    fs::write(
        &out_path,
        serde_json::to_string_pretty(&bundle).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    println!("Packed {name} {ver} -> {}", out_path.display());
    Ok(())
}

pub(crate) fn unpack_bundle_to_dir(bundle: &JsonValue, dest: &Path) -> Result<(), String> {
    verify_bundle_json(bundle)?;
    if dest.exists() {
        fs::remove_dir_all(dest).map_err(|e| e.to_string())?;
    }
    fs::create_dir_all(dest).map_err(|e| e.to_string())?;
    let files = bundle["files"].as_object().ok_or("bundle missing files")?;
    for (rel, b64) in files {
        let safe = safe_package_relative_path(rel)?;
        let out = dest.join(safe);
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64.as_str().unwrap_or(""))
            .map_err(|e| e.to_string())?;
        fs::write(out, bytes).map_err(|e| e.to_string())?;
    }
    if let Some(sig) = bundle.get("signature").filter(|s| !s.is_null()) {
        fs::write(
            dest.join("kovrek.sig.json"),
            serde_json::to_string_pretty(sig).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        let _ = verify_package_dir_sig(dest)?;
    }
    Ok(())
}

pub(crate) fn pkg_unpack(bundle_path: &Path, dest: Option<&Path>) -> Result<(), String> {
    let bundle: JsonValue =
        serde_json::from_str(&fs::read_to_string(bundle_path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let name = bundle
        .get("name")
        .and_then(JsonValue::as_str)
        .unwrap_or("package");
    let out_dir = dest
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(name));
    unpack_bundle_to_dir(&bundle, &out_dir)?;
    println!("Unpacked {name} -> {}", out_dir.display());
    Ok(())
}

pub(crate) fn pkg_build_plugin(dir: Option<&Path>) -> Result<(), String> {
    let target = match dir {
        Some(d) => d.to_path_buf(),
        None => project_root(&env::current_dir().map_err(|e| e.to_string())?)?,
    };
    let manifest: JsonValue = serde_json::from_str(
        &fs::read_to_string(target.join("kovrek.json")).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let native_root = manifest
        .get("native")
        .ok_or("kovrek.json has no 'native' plugin configuration")?;
    let native = select_native_spec(native_root).ok_or_else(|| {
        format!(
            "no 'native' plugin config for platform '{}'",
            current_platform_key()
        )
    })?;
    let src_rel = native
        .get("source")
        .and_then(JsonValue::as_str)
        .ok_or("kovrek.json 'native.source' is required")?;
    let raw_lib = native
        .get("library")
        .and_then(JsonValue::as_str)
        .unwrap_or("plugin");
    let mut out_path = target.join(safe_package_relative_path(raw_lib)?);
    if out_path.extension().is_none() {
        out_path.set_extension(crate::plugin::platform_shared_ext());
    }
    let src_path = target.join(safe_package_relative_path(src_rel)?);
    let built = crate::plugin::build_rust_plugin(&src_path, &out_path)?;
    println!("Built Kovrek native plugin: {}", built.display());
    Ok(())
}

/// Generate a sovereign Kovrek bridge package wrapping a Python/PyPI module via CPython C-API.
pub(crate) fn pkg_py_bridge(
    pypi_name: &str,
    out_dir: Option<&Path>,
    pip_install: bool,
) -> Result<(), String> {
    let mod_name = pypi_name.trim();
    if mod_name.is_empty()
        || mod_name.starts_with('.')
        || !mod_name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        return Err(format!("invalid Python/PyPI package name '{pypi_name}'"));
    }
    if pip_install {
        let mut installed = false;
        let mut last_err = String::new();
        for py in ["python", "python3", "py"] {
            match std::process::Command::new(py)
                .args(["-m", "pip", "install", mod_name])
                .status()
            {
                Ok(st) if st.success() => {
                    installed = true;
                    break;
                }
                Ok(st) => last_err = format!("{py} -m pip install exited with {st}"),
                Err(e) => last_err = e.to_string(),
            }
        }
        if !installed {
            return Err(format!("failed to pip install '{mod_name}': {last_err}"));
        }
    }

    let import_name = mod_name.replace('-', "_");
    let sanitized: String = mod_name
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let vy_name = format!("py_{sanitized}");
    let entry_file = format!("{vy_name}.kvr");

    let cwd = env::current_dir().map_err(|e| e.to_string())?;
    let active_project = project_root(&cwd).ok();
    let (dest, register_root) = match out_dir {
        Some(dir) => (dir.to_path_buf(), None),
        None => match active_project {
            Some(ref root) => (vendor_pkg_dir(root, &vy_name), Some(root.clone())),
            None => (cwd.join(&vy_name), None),
        },
    };
    fs::create_dir_all(&dest).map_err(|e| e.to_string())?;

    let manifest = serde_json::json!({
        "name": vy_name,
        "version": "0.1.0",
        "entry": entry_file,
        "description": format!("Kovrek CPython C-API bridge wrapper for Python module '{mod_name}'"),
        "python_package": mod_name,
        "python_module": import_name,
        "packages": {}
    });
    fs::write(
        dest.join("kovrek.json"),
        serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;

    let vy_source = format!(
        r#"// Auto-generated by `kovrek pkg py-bridge {mod_name}`

fn available() -> Bool {{
    return py_available();
}}

fn module() -> Any {{
    return py_import("{import_name}");
}}

fn get_attr(attr: String) -> Any {{
    return py_get(py_import("{import_name}"), attr);
}}

fn set_attr(attr: String, value: Any) -> Any {{
    return py_set(py_import("{import_name}"), attr, value);
}}

fn call(func_name: String, args: List) -> Any {{
    let m = py_import("{import_name}");
    let target = py_get(m, func_name);
    return py_call(target, args);
}}

fn call_kw(func_name: String, args: List, kwargs: Map) -> Any {{
    let m = py_import("{import_name}");
    let target = py_get(m, func_name);
    return py_call(target, args, kwargs);
}}

fn to_tensor(handle: Any) -> Any {{
    return py_to_tensor(handle);
}}

fn exports() -> List {{
    return py_dir(py_import("{import_name}"));
}}

fn info() -> Map {{
    return {{
        "bridge": "cpython-c-api",
        "python_package": "{mod_name}",
        "python_module": "{import_name}",
        "available": py_available(),
        "runtime": py_info()
    }};
}}
"#
    );
    fs::write(dest.join(&entry_file), vy_source).map_err(|e| e.to_string())?;

    if let Some(root) = register_root {
        let proj_manifest_path = root.join("kovrek.json");
        let mut proj_manifest: JsonValue = serde_json::from_str(
            &fs::read_to_string(&proj_manifest_path).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        if !proj_manifest
            .get("packages")
            .map(JsonValue::is_object)
            .unwrap_or(false)
        {
            proj_manifest["packages"] = JsonValue::Object(serde_json::Map::new());
        }
        proj_manifest["packages"][&vy_name] = serde_json::json!({
            "version": "0.1.0",
            "entry": entry_file,
            "python_package": mod_name
        });
        fs::write(
            &proj_manifest_path,
            serde_json::to_string_pretty(&proj_manifest).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        write_package_lock(&root, &proj_manifest)?;
    }

    println!(
        "Generated Python C-API bridge '{vy_name}' for '{mod_name}' -> {}",
        dest.display()
    );
    Ok(())
}
