// Veyra Native Binary Extension ABI (`veyra_plugin_v1`):
// Enables self-standing compiled shared libraries (`.dll`, `.so`, `.dylib`) written
// in Rust (`cdylib`), C, or Zig to plug natively into Veyra programs and Veyra packages
// without depending on Python's CPython `.pyd` ABI.

use std::{
    collections::HashMap,
    ffi::{CStr, CString, c_char},
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex, OnceLock},
};

use libloading::{Library, Symbol};
use serde_json::Value as JsonValue;

use crate::value::{Val, json_to_val, val_to_json};

pub const VEYRA_PLUGIN_ABI_VERSION: u32 = 1;

#[derive(Clone, Debug)]
pub(crate) struct PluginExport {
    pub(crate) symbol: String,
    pub(crate) kind: String,
    pub(crate) arity: Option<usize>,
}

pub(crate) struct LoadedPlugin {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) path: PathBuf,
    pub(crate) abi_version: u32,
    pub(crate) exports: HashMap<String, PluginExport>,
    library: Arc<Library>,
}

impl LoadedPlugin {
    pub(crate) fn info_val(&self) -> Val {
        let mut fn_names: Vec<String> = self.exports.keys().cloned().collect();
        fn_names.sort();
        Val::Map(vec![
            (Val::Str("id".into()), Val::Str(self.id.clone())),
            (Val::Str("name".into()), Val::Str(self.name.clone())),
            (Val::Str("version".into()), Val::Str(self.version.clone())),
            (
                Val::Str("path".into()),
                Val::Str(self.path.to_string_lossy().to_string()),
            ),
            (
                Val::Str("abi_version".into()),
                Val::Num(self.abi_version as f64),
            ),
            (
                Val::Str("functions".into()),
                Val::List(fn_names.into_iter().map(Val::Str).collect()),
            ),
        ])
    }

    pub(crate) fn call(&self, fn_name: &str, args: &[Val]) -> Result<Val, String> {
        let exp = self
            .exports
            .get(fn_name)
            .ok_or_else(|| format!("plugin '{}' has no function '{fn_name}'", self.name))?;
        if let Some(expected) = exp.arity {
            if args.len() != expected {
                return Err(format!(
                    "plugin function '{fn_name}' expects {expected} arguments, got {}",
                    args.len()
                ));
            }
        }
        let mut sym = exp.symbol.as_bytes().to_vec();
        sym.push(0);

        match exp.kind.as_str() {
            "json" => {
                let json_args =
                    serde_json::to_string(&args.iter().map(val_to_json).collect::<Vec<_>>())
                        .map_err(|e| e.to_string())?;
                let c_args = CString::new(json_args).map_err(|e| e.to_string())?;
                let func: Symbol<unsafe extern "C" fn(*const c_char) -> *mut c_char> =
                    unsafe { self.library.get(&sym) }.map_err(|e| e.to_string())?;
                let raw_ptr = unsafe { func(c_args.as_ptr()) };
                if raw_ptr.is_null() {
                    return Err(format!("plugin function '{fn_name}' returned null pointer"));
                }
                let out_str = unsafe { CStr::from_ptr(raw_ptr) }
                    .to_string_lossy()
                    .to_string();
                if let Ok(free_fn) = unsafe {
                    self.library
                        .get::<unsafe extern "C" fn(*mut c_char)>(b"veyra_plugin_free\0")
                } {
                    unsafe { free_fn(raw_ptr) };
                }
                let parsed: JsonValue =
                    serde_json::from_str(&out_str).map_err(|e| e.to_string())?;
                if let Some(err) = parsed.get("err").and_then(JsonValue::as_str) {
                    return Err(err.to_string());
                }
                if let Some(ok_val) = parsed.get("ok") {
                    return Ok(json_to_val(ok_val));
                }
                Ok(json_to_val(&parsed))
            }
            "f64_slice" => {
                let xs: Vec<f64> = match args.first() {
                    Some(Val::List(items)) => items
                        .iter()
                        .map(|v| match v {
                            Val::Num(n) => Ok(*n),
                            _ => Err("f64_slice plugin expects numeric List".to_string()),
                        })
                        .collect::<Result<Vec<_>, _>>()?,
                    _ => return Err("f64_slice plugin expects numeric List as first arg".into()),
                };
                let mut out = vec![0.0f64; xs.len().max(1)];
                let func: Symbol<unsafe extern "C" fn(*const f64, usize, *mut f64) -> usize> =
                    unsafe { self.library.get(&sym) }.map_err(|e| e.to_string())?;
                let written = unsafe { func(xs.as_ptr(), xs.len(), out.as_mut_ptr()) };
                out.truncate(written.min(out.len()));
                Ok(Val::List(out.into_iter().map(Val::Num).collect()))
            }
            "f64" => {
                let nums: Vec<f64> = args
                    .iter()
                    .map(|v| match v {
                        Val::Num(n) => Ok(*n),
                        _ => Err("f64 plugin arguments must be Numbers".to_string()),
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let res = unsafe {
                    match nums.as_slice() {
                        [] => {
                            let f: Symbol<unsafe extern "C" fn() -> f64> =
                                self.library.get(&sym).map_err(|e| e.to_string())?;
                            f()
                        }
                        [a] => {
                            let f: Symbol<unsafe extern "C" fn(f64) -> f64> =
                                self.library.get(&sym).map_err(|e| e.to_string())?;
                            f(*a)
                        }
                        [a, b] => {
                            let f: Symbol<unsafe extern "C" fn(f64, f64) -> f64> =
                                self.library.get(&sym).map_err(|e| e.to_string())?;
                            f(*a, *b)
                        }
                        [a, b, c] => {
                            let f: Symbol<unsafe extern "C" fn(f64, f64, f64) -> f64> =
                                self.library.get(&sym).map_err(|e| e.to_string())?;
                            f(*a, *b, *c)
                        }
                        _ => return Err("f64 plugin supports 0..3 arguments".into()),
                    }
                };
                Ok(Val::Num(res))
            }
            "i64" => {
                let nums: Vec<i64> = args
                    .iter()
                    .map(|v| match v {
                        Val::Num(n) => Ok(*n as i64),
                        _ => Err("i64 plugin arguments must be Numbers".to_string()),
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let res = unsafe {
                    match nums.as_slice() {
                        [] => {
                            let f: Symbol<unsafe extern "C" fn() -> i64> =
                                self.library.get(&sym).map_err(|e| e.to_string())?;
                            f()
                        }
                        [a] => {
                            let f: Symbol<unsafe extern "C" fn(i64) -> i64> =
                                self.library.get(&sym).map_err(|e| e.to_string())?;
                            f(*a)
                        }
                        [a, b] => {
                            let f: Symbol<unsafe extern "C" fn(i64, i64) -> i64> =
                                self.library.get(&sym).map_err(|e| e.to_string())?;
                            f(*a, *b)
                        }
                        _ => return Err("i64 plugin supports 0..2 arguments".into()),
                    }
                };
                Ok(Val::Num(res as f64))
            }
            "i32" => {
                let nums: Vec<i32> = args
                    .iter()
                    .map(|v| match v {
                        Val::Num(n) => Ok(*n as i32),
                        _ => Err("i32 plugin arguments must be Numbers".to_string()),
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let res = unsafe {
                    match nums.as_slice() {
                        [] => {
                            let f: Symbol<unsafe extern "C" fn() -> i32> =
                                self.library.get(&sym).map_err(|e| e.to_string())?;
                            f()
                        }
                        [a] => {
                            let f: Symbol<unsafe extern "C" fn(i32) -> i32> =
                                self.library.get(&sym).map_err(|e| e.to_string())?;
                            f(*a)
                        }
                        [a, b] => {
                            let f: Symbol<unsafe extern "C" fn(i32, i32) -> i32> =
                                self.library.get(&sym).map_err(|e| e.to_string())?;
                            f(*a, *b)
                        }
                        [a, b, c] => {
                            let f: Symbol<unsafe extern "C" fn(i32, i32, i32) -> i32> =
                                self.library.get(&sym).map_err(|e| e.to_string())?;
                            f(*a, *b, *c)
                        }
                        _ => return Err("i32 plugin supports 0..3 arguments".into()),
                    }
                };
                Ok(Val::Num(res as f64))
            }
            other => Err(format!("unsupported plugin export kind '{other}'")),
        }
    }
}

static PLUGINS: OnceLock<Mutex<HashMap<String, Arc<LoadedPlugin>>>> = OnceLock::new();

fn plugin_registry() -> &'static Mutex<HashMap<String, Arc<LoadedPlugin>>> {
    PLUGINS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn platform_shared_ext() -> &'static str {
    if cfg!(windows) {
        "dll"
    } else if cfg!(target_os = "macos") {
        "dylib"
    } else {
        "so"
    }
}

pub(crate) fn load_plugin(path: &Path) -> Result<Arc<LoadedPlugin>, String> {
    let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let key = canon.to_string_lossy().to_string();
    {
        let guard = plugin_registry()
            .lock()
            .map_err(|_| "plugin registry poisoned")?;
        if let Some(p) = guard.get(&key) {
            return Ok(p.clone());
        }
    }

    let library = Arc::new(unsafe { Library::new(&canon) }.map_err(|e| e.to_string())?);
    let abi_fn: Symbol<unsafe extern "C" fn() -> u32> = unsafe {
        library
            .get(b"veyra_plugin_abi_version\0")
            .map_err(|_| "shared library is missing 'veyra_plugin_abi_version'".to_string())?
    };
    let abi_version = unsafe { abi_fn() };
    if abi_version != VEYRA_PLUGIN_ABI_VERSION {
        return Err(format!(
            "unsupported Veyra plugin ABI version {abi_version} (expected {VEYRA_PLUGIN_ABI_VERSION})"
        ));
    }
    let manifest_fn: Symbol<unsafe extern "C" fn() -> *const c_char> = unsafe {
        library
            .get(b"veyra_plugin_manifest\0")
            .map_err(|_| "shared library is missing 'veyra_plugin_manifest'".to_string())?
    };
    let manifest_ptr = unsafe { manifest_fn() };
    if manifest_ptr.is_null() {
        return Err("veyra_plugin_manifest returned null".into());
    }
    let manifest_str = unsafe { CStr::from_ptr(manifest_ptr) }
        .to_string_lossy()
        .to_string();
    let manifest: JsonValue = serde_json::from_str(&manifest_str)
        .map_err(|e| format!("invalid plugin manifest JSON: {e}"))?;
    let name = manifest
        .get("name")
        .and_then(JsonValue::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("veyra-plugin")
        .to_string();
    let version = manifest
        .get("version")
        .and_then(JsonValue::as_str)
        .unwrap_or("0.1.0")
        .to_string();

    let mut exports = HashMap::new();
    if let Some(fns) = manifest.get("functions").and_then(JsonValue::as_array) {
        for item in fns {
            let Some(fn_name) = item.get("name").and_then(JsonValue::as_str) else {
                continue;
            };
            let symbol = item
                .get("symbol")
                .and_then(JsonValue::as_str)
                .unwrap_or(fn_name)
                .to_string();
            let kind = item
                .get("kind")
                .and_then(JsonValue::as_str)
                .unwrap_or("json")
                .to_string();
            let arity = item
                .get("arity")
                .and_then(JsonValue::as_u64)
                .map(|v| v as usize);
            exports.insert(
                fn_name.to_string(),
                PluginExport {
                    symbol,
                    kind,
                    arity,
                },
            );
        }
    }

    let plugin = Arc::new(LoadedPlugin {
        id: key.clone(),
        name: name.clone(),
        version,
        path: canon,
        abi_version,
        exports,
        library,
    });
    let mut guard = plugin_registry()
        .lock()
        .map_err(|_| "plugin registry poisoned")?;
    guard.insert(key, plugin.clone());
    guard.entry(name).or_insert_with(|| plugin.clone());
    Ok(plugin)
}

pub(crate) fn get_loaded_plugin(name_or_id: &str) -> Result<Arc<LoadedPlugin>, String> {
    let guard = plugin_registry()
        .lock()
        .map_err(|_| "plugin registry poisoned")?;
    if let Some(p) = guard.get(name_or_id) {
        return Ok(p.clone());
    }
    let path_key = Path::new(name_or_id)
        .canonicalize()
        .ok()
        .map(|p| p.to_string_lossy().to_string());
    if let Some(pk) = path_key {
        if let Some(p) = guard.get(&pk) {
            return Ok(p.clone());
        }
    }
    Err(format!("plugin '{name_or_id}' is not loaded"))
}

pub(crate) fn list_loaded_plugins() -> Vec<Val> {
    let Ok(guard) = plugin_registry().lock() else {
        return Vec::new();
    };
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for p in guard.values() {
        if seen.insert(p.id.clone()) {
            out.push(p.info_val());
        }
    }
    out
}

pub(crate) fn build_rust_plugin(src_path: &Path, out_path: &Path) -> Result<PathBuf, String> {
    if let Some(parent) = out_path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let out = Command::new("rustc")
        .arg("--edition")
        .arg("2021")
        .arg("--crate-type")
        .arg("cdylib")
        .arg("-C")
        .arg("opt-level=3")
        .arg(src_path)
        .arg("-o")
        .arg(out_path)
        .output()
        .map_err(|e| format!("failed to launch rustc for Veyra plugin build: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(format!("plugin rustc compilation failed: {stderr}"));
    }
    Ok(out_path.to_path_buf())
}
