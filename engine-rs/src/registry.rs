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
use crate::project::{copy_dir_all, project_root, veyra_home};
use crate::value::{constant_time_strings_equal, hmac_sha256_hex};

pub(crate) fn pkg_index() -> Result<(PathBuf, JsonValue), String> {
    if let Ok(url) = env::var("VEYRA_REGISTRY_URL") {
        #[cfg(feature = "native-services")]
        {
            let target_url = if url.ends_with(".json") {
                url.clone()
            } else {
                format!("{}/index.json", url.trim_end_matches('/'))
            };
            let response = reqwest::blocking::get(&target_url).map_err(|e| e.to_string())?;
            if !response.status().is_success() {
                return Err(format!(
                    "remote registry returned HTTP {}",
                    response.status()
                ));
            }
            let text = response.text().map_err(|e| e.to_string())?;
            let json = serde_json::from_str(&text).map_err(|e| e.to_string())?;
            return Ok((PathBuf::from(target_url), json));
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

#[cfg(feature = "native-services")]
pub(crate) fn fetch_package_file(url: &str) -> Result<Vec<u8>, String> {
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

pub(crate) fn write_package_lock(root: &Path, manifest: &JsonValue) -> Result<(), String> {
    let mut locked = serde_json::Map::new();
    if let Some(packages) = manifest.get("packages").and_then(JsonValue::as_object) {
        for (name, meta) in packages {
            let mut entry = meta.clone();
            let pkg_dir = root.join("vendor").join(name);
            let hash = hash_directory(&pkg_dir)?;
            entry["sha256"] = JsonValue::String(hash);
            if pkg_dir.join("veyra.sig.json").is_file() {
                entry["signed"] = JsonValue::Bool(true);
            }
            if let Ok(m_txt) = fs::read_to_string(pkg_dir.join("veyra.json")) {
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
        root.join("veyra.lock"),
        serde_json::to_string_pretty(&lock).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())
}

pub(crate) fn pkg_add_inner(spec: &str, seen: &mut HashSet<String>) -> Result<(), String> {
    let (name, constraint) = match spec.split_once('@') {
        Some((n, c)) => (n.trim(), Some(c.trim())),
        None => (spec.trim(), None),
    };
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
    if let Some(req) = constraint {
        if !semver_matches(version, req) {
            return Err(format!(
                "package '{name}' version {version} does not match requested constraint '{req}'"
            ));
        }
    }
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
    } else if let Some(bundle_rel) = pkg.get("bundle_path").and_then(JsonValue::as_str) {
        #[cfg(feature = "native-services")]
        {
            let bundle_url =
                if bundle_rel.starts_with("http://") || bundle_rel.starts_with("https://") {
                    bundle_rel.to_string()
                } else {
                    let base = env::var("VEYRA_REGISTRY_URL")
                        .map_err(|_| "bundle_path requires VEYRA_REGISTRY_URL".to_string())?;
                    format!(
                        "{}/{}",
                        base.trim_end_matches('/'),
                        bundle_rel.trim_start_matches('/')
                    )
                };
            let bytes = fetch_package_file(&bundle_url)?;
            let bundle: JsonValue = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
            unpack_bundle_to_dir(&bundle, &dest)?;
        }
        #[cfg(not(feature = "native-services"))]
        {
            let _ = bundle_rel;
            return Err("remote bundle download requires the native-services feature".into());
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
    if dest.join("veyra.sig.json").is_file() {
        let _ = verify_package_dir_sig(&dest)?;
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

pub(crate) fn pkg_add(name: &str) -> Result<(), String> {
    pkg_add_inner(name, &mut HashSet::new())
}

pub(crate) fn pkg_remove(name: &str) -> Result<(), String> {
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

pub(crate) fn pkg_list() -> Result<(), String> {
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

pub(crate) fn pkg_install() -> Result<(), String> {
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

pub(crate) fn pkg_search(term: &str) -> Result<(), String> {
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

pub(crate) fn pkg_publish() -> Result<(), String> {
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
    if root.join("src").is_dir() {
        copy_dir_all(&root.join("src"), &pkg_root.join("src"))?;
    }
    let _ = fs::copy(&manifest_path, pkg_root.join("veyra.json"));
    if root.join("veyra.sig.json").is_file() {
        let _ = fs::copy(root.join("veyra.sig.json"), pkg_root.join("veyra.sig.json"));
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

    if let Ok(remote_url) = env::var("VEYRA_REGISTRY_URL") {
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
    let mut signed = 0usize;
    for (name, meta) in packages {
        let expected = meta
            .get("sha256")
            .and_then(JsonValue::as_str)
            .ok_or(format!("package {name} has no sha256"))?;
        let pkg_dir = root.join("vendor").join(name);
        let actual = hash_directory(&pkg_dir)?;
        if actual != expected {
            return Err(format!("package integrity mismatch: {name}"));
        }
        if pkg_dir.join("veyra.sig.json").is_file() {
            if verify_package_dir_sig(&pkg_dir)? {
                signed += 1;
            }
        }
        checked += 1;
    }
    if signed > 0 {
        println!(
            "Audit OK: {checked} package(s), content hashes verified ({signed} cryptographically signed)"
        );
    } else {
        println!("Audit OK: {checked} package(s), content hashes verified");
    }
    Ok(())
}

fn keys_dir() -> PathBuf {
    veyra_home().join("keys")
}

fn find_signing_key(key_name: Option<&str>) -> Result<(String, String), String> {
    if let Ok(secret) = env::var("VEYRA_SIGNING_KEY") {
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
    Err("no Veyra signing key found; run 'veyra pkg keygen [name]' first".into())
}

fn find_trusted_secret_by_id(key_id: &str) -> Option<String> {
    if let Ok(secret) = env::var("VEYRA_SIGNING_KEY") {
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
        if p.file_name().and_then(|s| s.to_str()) == Some("veyra.sig.json") {
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
    let sig_path = dir.join("veyra.sig.json");
    if !sig_path.is_file() {
        return Ok(false);
    }
    let sig: JsonValue =
        serde_json::from_str(&fs::read_to_string(&sig_path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let expected_sha = sig
        .get("sha256")
        .and_then(JsonValue::as_str)
        .ok_or("veyra.sig.json missing sha256")?;
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
        .ok_or("veyra.sig.json missing key_id")?;
    let signature = sig
        .get("signature")
        .and_then(JsonValue::as_str)
        .ok_or("veyra.sig.json missing signature")?;
    if let Some(secret) = find_trusted_secret_by_id(key_id) {
        let expected_sig = hmac_sha256_hex(&secret, &format!("veyra-pkg-v1:{expected_sha}"));
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
        "Generated Veyra signing key '{key_name}' (key_id={key_id}) -> {}",
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
    let signature = hmac_sha256_hex(&secret, &format!("veyra-pkg-v1:{sha}"));
    let (pkg_name, version) = fs::read_to_string(target.join("veyra.json"))
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
    let sig_path = target.join("veyra.sig.json");
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
        let root = project_root(&env::current_dir().map_err(|e| e.to_string())?)?;
        let pkg_dir = root.join("vendor").join(t);
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
        .ok_or_else(|| format!("package '{name}' not found in Veyra registry"))?;
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
            "yes (veyra_plugin_v1)"
        } else {
            "no (pure Veyra / VM)"
        }
    );
    Ok(())
}

pub(crate) fn pkg_tree() -> Result<(), String> {
    let root = project_root(&env::current_dir().map_err(|e| e.to_string())?)?;
    let manifest: JsonValue = serde_json::from_str(
        &fs::read_to_string(root.join("veyra.json")).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let proj_name = manifest
        .get("name")
        .and_then(JsonValue::as_str)
        .unwrap_or("veyra-app");
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
        let pkg_dir = root.join("vendor").join(name);
        let ver = index
            .get("packages")
            .and_then(|p| p.get(name))
            .and_then(|m| m.get("version"))
            .and_then(JsonValue::as_str)
            .unwrap_or("?");
        let mut tags = Vec::new();
        if pkg_dir.join("veyra.sig.json").is_file() {
            tags.push("signed");
        }
        if let Ok(t) = fs::read_to_string(pkg_dir.join("veyra.json")) {
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
        &fs::read_to_string(root.join("veyra.json")).map_err(|e| e.to_string())?,
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
        &fs::read_to_string(dir.join("veyra.json")).unwrap_or_else(|_| {
            serde_json::json!({"name":"package","version":"0.1.0","entry":"src/main.vy"})
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
        .unwrap_or("src/main.vy");
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
        if rel == "veyra.lock" || rel.ends_with(".vypkg.json") {
            continue;
        }
        let bytes = fs::read(&p).map_err(|e| e.to_string())?;
        encoded.insert(
            rel,
            JsonValue::String(base64::engine::general_purpose::STANDARD.encode(bytes)),
        );
    }
    let sha256 = hash_directory_excluding_sig(dir)?;
    let sig = if dir.join("veyra.sig.json").is_file() {
        serde_json::from_str::<JsonValue>(
            &fs::read_to_string(dir.join("veyra.sig.json")).map_err(|e| e.to_string())?,
        )
        .ok()
    } else {
        None
    };
    Ok(serde_json::json!({
        "format": "veyra.bundle.v1",
        "name": name,
        "version": version,
        "entry": entry,
        "dependencies": manifest.get("dependencies").cloned().unwrap_or_else(|| serde_json::json!([])),
        "native": manifest.get("native").cloned().unwrap_or(JsonValue::Null),
        "sha256": sha256,
        "signature": sig,
        "files": encoded
    }))
}

fn verify_bundle_json(bundle: &JsonValue) -> Result<(), String> {
    if bundle.get("format").and_then(JsonValue::as_str) != Some("veyra.bundle.v1") {
        return Err("unsupported .vypkg bundle format".into());
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
        .unwrap_or_else(|| target.join(format!("{name}-{ver}.vypkg.json")));
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
            dest.join("veyra.sig.json"),
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
        &fs::read_to_string(target.join("veyra.json")).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let native = manifest
        .get("native")
        .ok_or("veyra.json has no 'native' plugin configuration")?;
    let src_rel = native
        .get("source")
        .and_then(JsonValue::as_str)
        .ok_or("veyra.json 'native.source' is required")?;
    let raw_lib = native
        .get("library")
        .and_then(JsonValue::as_str)
        .unwrap_or("plugin");
    let mut out_path = target.join(raw_lib);
    if out_path.extension().is_none() {
        out_path.set_extension(crate::plugin::platform_shared_ext());
    }
    let src_path = target.join(src_rel);
    let built = crate::plugin::build_rust_plugin(&src_path, &out_path)?;
    println!("Built Veyra native plugin: {}", built.display());
    Ok(())
}
