// Package registry: the project index, integrity hashing, the lockfile and the
// add/remove/list/install/search/publish/audit commands.
//
// Extracted from `main.rs` without behavior change; see docs/ENGINE-SPLIT.md.

use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Component, Path, PathBuf};

use serde_json::Value as JsonValue;
use sha2::{Digest, Sha256};

use crate::VERSION;
use crate::project::{copy_dir_all, project_root, veyra_home};

pub(crate) fn pkg_index() -> Result<(PathBuf, JsonValue), String> {
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

pub(crate) fn write_package_lock(root: &Path, manifest: &JsonValue) -> Result<(), String> {
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

pub(crate) fn pkg_add_inner(name: &str, seen: &mut HashSet<String>) -> Result<(), String> {
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
