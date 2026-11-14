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

// =========================================================================
// Pure-Rust C Header (`*.h`) FFI Bindgen & Native C-ABI Dispatch
// =========================================================================

static RAW_FFI_LIBS: OnceLock<Mutex<HashMap<String, Arc<Library>>>> = OnceLock::new();

fn raw_ffi_libs() -> &'static Mutex<HashMap<String, Arc<Library>>> {
    RAW_FFI_LIBS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn load_raw_ffi_lib(path: &str) -> Result<Arc<Library>, String> {
    let canon = Path::new(path)
        .canonicalize()
        .unwrap_or_else(|_| Path::new(path).to_path_buf());
    let key = canon.to_string_lossy().to_string();
    {
        let guard = raw_ffi_libs()
            .lock()
            .map_err(|_| "ffi library cache poisoned")?;
        if let Some(lib) = guard.get(&key) {
            return Ok(lib.clone());
        }
    }
    let lib = Arc::new(
        unsafe { Library::new(&canon) }
            .or_else(|_| unsafe { Library::new(path) })
            .map_err(|e| format!("ffi_call failed loading '{path}': {e}"))?,
    );
    let mut guard = raw_ffi_libs()
        .lock()
        .map_err(|_| "ffi library cache poisoned")?;
    guard.insert(key, lib.clone());
    Ok(lib)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CAbiType {
    Void,
    I32,
    U32,
    I64,
    U64,
    F32,
    F64,
    Bool,
    CStr,
    Ptr,
}

impl CAbiType {
    fn tag(self) -> &'static str {
        match self {
            CAbiType::Void => "void",
            CAbiType::I32 => "i32",
            CAbiType::U32 => "u32",
            CAbiType::I64 => "i64",
            CAbiType::U64 => "u64",
            CAbiType::F32 => "f32",
            CAbiType::F64 => "f64",
            CAbiType::Bool => "bool",
            CAbiType::CStr => "cstr",
            CAbiType::Ptr => "ptr",
        }
    }

    fn veyra_type(self) -> &'static str {
        match self {
            CAbiType::Void => "Null",
            CAbiType::I32 | CAbiType::U32 | CAbiType::I64 | CAbiType::U64 | CAbiType::Ptr => "Int",
            CAbiType::F32 | CAbiType::F64 => "Float",
            CAbiType::Bool => "Bool",
            CAbiType::CStr => "String",
        }
    }

    fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "void" => Ok(CAbiType::Void),
            "i32" | "int" | "c_int" => Ok(CAbiType::I32),
            "u32" | "unsigned" | "uint" | "c_uint" => Ok(CAbiType::U32),
            "i64" | "long" | "isize" | "ssize_t" | "ptrdiff_t" => Ok(CAbiType::I64),
            "u64" | "usize" | "size_t" => Ok(CAbiType::U64),
            "f32" | "float" => Ok(CAbiType::F32),
            "f64" | "double" => Ok(CAbiType::F64),
            "bool" | "_bool" => Ok(CAbiType::Bool),
            "cstr" | "str" | "char*" | "const char*" => Ok(CAbiType::CStr),
            "ptr" | "void*" | "const void*" => Ok(CAbiType::Ptr),
            other => Err(format!("unsupported C ABI type '{other}'")),
        }
    }
}

fn strip_c_comments(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    let mut in_str = false;
    while i < bytes.len() {
        if !in_str && i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                if bytes[i] == b'\n' {
                    out.push('\n');
                }
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            continue;
        }
        if !in_str && i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if bytes[i] == b'"' && (i == 0 || bytes[i - 1] != b'\\') {
            in_str = !in_str;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn map_c_type(raw: &str, typedefs: &HashMap<String, CAbiType>) -> CAbiType {
    let mut cleaned = raw.trim().replace('*', " * ");
    for qual in [
        "const",
        "volatile",
        "restrict",
        "extern",
        "static",
        "inline",
        "__inline",
        "__cdecl",
        "__stdcall",
    ] {
        cleaned = cleaned
            .split_whitespace()
            .filter(|w| *w != qual)
            .collect::<Vec<_>>()
            .join(" ");
    }
    let norm = cleaned.trim();
    if let Some(&ty) = typedefs.get(norm) {
        return ty;
    }
    if norm.contains('*') {
        let base = norm.replace('*', "").trim().to_string();
        if base == "char"
            || base == "const char"
            || base == "signed char"
            || base == "unsigned char"
        {
            if !norm.contains("**") {
                return CAbiType::CStr;
            }
        }
        return CAbiType::Ptr;
    }
    match norm {
        "void" => CAbiType::Void,
        "float" => CAbiType::F32,
        "double" | "long double" => CAbiType::F64,
        "bool" | "_Bool" => CAbiType::Bool,
        "int8_t" | "int16_t" | "int32_t" | "short" | "short int" | "signed short" | "int"
        | "signed" | "signed int" | "char" | "signed char" => CAbiType::I32,
        "uint8_t" | "uint16_t" | "uint32_t" | "unsigned char" | "unsigned short"
        | "unsigned short int" | "unsigned" | "unsigned int" => CAbiType::U32,
        "int64_t" | "long long" | "long long int" | "signed long long" | "long" | "long int"
        | "signed long" | "ssize_t" | "intptr_t" | "ptrdiff_t" => CAbiType::I64,
        "uint64_t"
        | "unsigned long long"
        | "unsigned long long int"
        | "unsigned long"
        | "unsigned long int"
        | "size_t"
        | "uintptr_t" => CAbiType::U64,
        other => {
            if let Some(first) = other.split_whitespace().next()
                && let Some(&ty) = typedefs.get(first)
            {
                return ty;
            }
            CAbiType::I64
        }
    }
}

fn parse_c_param(
    raw: &str,
    idx: usize,
    typedefs: &HashMap<String, CAbiType>,
) -> (String, CAbiType) {
    let trimmed = raw.trim();
    if let Some(last_star) = trimmed.rfind('*') {
        let after = trimmed[last_star + 1..].trim();
        let clean_name: String = after
            .trim_end_matches("[]")
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        let ty_str = if clean_name.is_empty() {
            trimmed
        } else {
            &trimmed[..trimmed.len() - after.len()]
        };
        let ty = map_c_type(ty_str, typedefs);
        let name = if clean_name.is_empty() {
            format!("arg{idx}")
        } else {
            clean_name
        };
        return (name, ty);
    }
    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    if parts.len() >= 2 {
        let candidate = parts[parts.len() - 1].trim_end_matches("[]");
        if candidate
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !matches!(
                candidate,
                "int"
                    | "char"
                    | "long"
                    | "short"
                    | "float"
                    | "double"
                    | "void"
                    | "signed"
                    | "unsigned"
            )
        {
            let ty_str = parts[..parts.len() - 1].join(" ");
            return (candidate.to_string(), map_c_type(&ty_str, typedefs));
        }
    }
    (format!("arg{idx}"), map_c_type(trimmed, typedefs))
}

#[derive(Debug, Clone)]
pub(crate) struct CHeaderFn {
    pub(crate) name: String,
    ret: CAbiType,
    params: Vec<(String, CAbiType)>,
}

pub(crate) fn parse_c_header_functions(header_src: &str) -> Vec<CHeaderFn> {
    let stripped = strip_c_comments(header_src);
    let mut logical = String::new();
    for line in stripped.lines() {
        let t = line.trim();
        if t.is_empty()
            || t.starts_with('#')
            || t.starts_with("extern \"C\"")
            || t == "{"
            || t == "}"
        {
            continue;
        }
        logical.push_str(t);
        logical.push(' ');
    }

    let mut typedefs: HashMap<String, CAbiType> = HashMap::new();
    let mut funcs = Vec::new();
    for raw_stmt in logical.split(';') {
        let stmt = raw_stmt.trim();
        if stmt.is_empty() || stmt.contains('{') {
            continue;
        }
        if let Some(rest) = stmt.strip_prefix("typedef ") {
            if !rest.contains('(') {
                let parts: Vec<&str> = rest.split_whitespace().collect();
                if parts.len() >= 2 {
                    let alias = parts[parts.len() - 1].trim_matches('*').to_string();
                    let target_src = parts[..parts.len() - 1].join(" ");
                    let mapped = map_c_type(&target_src, &typedefs);
                    typedefs.insert(alias, mapped);
                }
            }
            continue;
        }
        if stmt.starts_with("struct ") && !stmt.contains('(') {
            continue;
        }
        let Some(open) = stmt.find('(') else {
            continue;
        };
        let Some(close) = stmt.rfind(')') else {
            continue;
        };
        if open >= close {
            continue;
        }
        let head = stmt[..open].trim();
        let params_raw = stmt[open + 1..close].trim();
        let head_norm = head.replace('*', "* ");
        let head_tokens: Vec<&str> = head_norm.split_whitespace().collect();
        if head_tokens.len() < 2 {
            continue;
        }
        let fn_name = head_tokens[head_tokens.len() - 1].trim_start_matches('*');
        if fn_name.is_empty()
            || !fn_name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            continue;
        }
        let ret_src = head_tokens[..head_tokens.len() - 1].join(" ");
        let ret = map_c_type(&ret_src, &typedefs);
        let mut params = Vec::new();
        if !params_raw.is_empty() && params_raw != "void" {
            for (idx, p) in params_raw.split(',').enumerate() {
                let pt = p.trim();
                if pt.is_empty() || pt == "..." {
                    continue;
                }
                params.push(parse_c_param(pt, idx, &typedefs));
            }
        }
        funcs.push(CHeaderFn {
            name: fn_name.to_string(),
            ret,
            params,
        });
    }
    funcs
}

pub(crate) fn ffi_bindgen(header_src: &str, lib_path: &str) -> Result<String, String> {
    let fns = parse_c_header_functions(header_src);
    if fns.is_empty() {
        return Err("ffi_bindgen found no C function declarations in header".into());
    }
    let escaped_lib = lib_path.replace('\\', "\\\\").replace('"', "\\\"");
    let mut out = String::new();
    out.push_str("// Auto-generated by veyra ffi bindgen\n");
    out.push_str(&format!("let __FFI_LIB = \"{escaped_lib}\";\n\n"));
    for f in fns {
        let sig_params = f
            .params
            .iter()
            .map(|(n, ty)| format!("{n}: {}", ty.veyra_type()))
            .collect::<Vec<_>>()
            .join(", ");
        let ptys = f
            .params
            .iter()
            .map(|(_, ty)| format!("\"{}\"", ty.tag()))
            .collect::<Vec<_>>()
            .join(", ");
        let arg_names = f
            .params
            .iter()
            .map(|(n, _)| n.clone())
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!(
            "fn {}({sig_params}) -> {} {{\n",
            f.name,
            f.ret.veyra_type()
        ));
        if f.ret == CAbiType::Void {
            out.push_str(&format!(
                "    ffi_call(__FFI_LIB, \"{}\", \"void\", [{ptys}], [{arg_names}]);\n",
                f.name
            ));
            out.push_str("    return null;\n}\n\n");
        } else {
            out.push_str(&format!(
                "    return ffi_call(__FFI_LIB, \"{}\", \"{}\", [{ptys}], [{arg_names}]);\n}}\n\n",
                f.name,
                f.ret.tag()
            ));
        }
    }
    Ok(out)
}

enum CArg {
    I32(i32),
    U32(u32),
    I64(i64),
    U64(u64),
    F32(f32),
    F64(f64),
    Bool(bool),
    CStr(CString),
    Ptr(*mut core::ffi::c_void),
}

fn val_to_carg(val: &Val, ty: CAbiType) -> Result<CArg, String> {
    match (ty, val) {
        (CAbiType::I32, Val::Num(n)) => Ok(CArg::I32(*n as i32)),
        (CAbiType::U32, Val::Num(n)) => Ok(CArg::U32(*n as u32)),
        (CAbiType::I64, Val::Num(n)) => Ok(CArg::I64(*n as i64)),
        (CAbiType::U64, Val::Num(n)) => Ok(CArg::U64(*n as u64)),
        (CAbiType::Ptr, Val::Num(n)) => Ok(CArg::Ptr((*n as usize) as *mut core::ffi::c_void)),
        (CAbiType::Ptr, Val::Null) => Ok(CArg::Ptr(std::ptr::null_mut())),
        (CAbiType::F32, Val::Num(n)) => Ok(CArg::F32(*n as f32)),
        (CAbiType::F64, Val::Num(n)) => Ok(CArg::F64(*n)),
        (CAbiType::Bool, Val::Bool(b)) => Ok(CArg::Bool(*b)),
        (CAbiType::Bool, Val::Num(n)) => Ok(CArg::Bool(*n != 0.0)),
        (CAbiType::CStr, Val::Str(s)) => CString::new(s.as_str())
            .map(CArg::CStr)
            .map_err(|_| "ffi string contains interior NUL".into()),
        _ => Err(format!("ffi_call argument type mismatch for {}", ty.tag())),
    }
}

pub(crate) fn ffi_call(
    lib_path: &str,
    fn_name: &str,
    ret_ty_str: &str,
    param_ty_strs: &[String],
    args: &[Val],
) -> Result<Val, String> {
    if param_ty_strs.len() != args.len() {
        return Err(format!(
            "ffi_call '{fn_name}' expects {} arguments, got {}",
            param_ty_strs.len(),
            args.len()
        ));
    }
    let ret_ty = CAbiType::parse(ret_ty_str)?;
    let param_tys: Vec<CAbiType> = param_ty_strs
        .iter()
        .map(|s| CAbiType::parse(s))
        .collect::<Result<_, _>>()?;
    let cargs: Vec<CArg> = args
        .iter()
        .zip(&param_tys)
        .map(|(v, ty)| val_to_carg(v, *ty))
        .collect::<Result<_, _>>()?;

    let lib = load_raw_ffi_lib(lib_path)?;
    let sym = CString::new(fn_name).map_err(|_| "invalid C symbol name")?;
    let mut sym_bytes = sym.into_bytes_with_nul();
    sym_bytes.shrink_to_fit();

    unsafe {
        match (ret_ty, param_tys.as_slice()) {
            (CAbiType::Void, []) => {
                let f: Symbol<unsafe extern "C" fn()> =
                    lib.get(&sym_bytes).map_err(|e| e.to_string())?;
                f();
                Ok(Val::Null)
            }
            (CAbiType::I32, []) => {
                let f: Symbol<unsafe extern "C" fn() -> i32> =
                    lib.get(&sym_bytes).map_err(|e| e.to_string())?;
                Ok(Val::Num(f() as f64))
            }
            (CAbiType::I64, []) => {
                let f: Symbol<unsafe extern "C" fn() -> i64> =
                    lib.get(&sym_bytes).map_err(|e| e.to_string())?;
                Ok(Val::Num(f() as f64))
            }
            (CAbiType::U64, []) => {
                let f: Symbol<unsafe extern "C" fn() -> u64> =
                    lib.get(&sym_bytes).map_err(|e| e.to_string())?;
                Ok(Val::Num(f() as f64))
            }
            (CAbiType::F64, []) => {
                let f: Symbol<unsafe extern "C" fn() -> f64> =
                    lib.get(&sym_bytes).map_err(|e| e.to_string())?;
                Ok(Val::Num(f()))
            }
            (CAbiType::Bool, []) => {
                let f: Symbol<unsafe extern "C" fn() -> bool> =
                    lib.get(&sym_bytes).map_err(|e| e.to_string())?;
                Ok(Val::Bool(f()))
            }
            (CAbiType::Bool, [CAbiType::Bool]) => {
                let f: Symbol<unsafe extern "C" fn(bool) -> bool> =
                    lib.get(&sym_bytes).map_err(|e| e.to_string())?;
                let CArg::Bool(a) = cargs[0] else {
                    unreachable!()
                };
                Ok(Val::Bool(f(a)))
            }
            (CAbiType::U32, []) => {
                let f: Symbol<unsafe extern "C" fn() -> u32> =
                    lib.get(&sym_bytes).map_err(|e| e.to_string())?;
                Ok(Val::Num(f() as f64))
            }
            (CAbiType::U32, [CAbiType::U32]) => {
                let f: Symbol<unsafe extern "C" fn(u32) -> u32> =
                    lib.get(&sym_bytes).map_err(|e| e.to_string())?;
                let CArg::U32(a) = cargs[0] else {
                    unreachable!()
                };
                Ok(Val::Num(f(a) as f64))
            }
            (CAbiType::U32, [CAbiType::U32, CAbiType::U32]) => {
                let f: Symbol<unsafe extern "C" fn(u32, u32) -> u32> =
                    lib.get(&sym_bytes).map_err(|e| e.to_string())?;
                let (CArg::U32(a), CArg::U32(b)) = (&cargs[0], &cargs[1]) else {
                    unreachable!()
                };
                Ok(Val::Num(f(*a, *b) as f64))
            }
            (CAbiType::Ptr, []) => {
                let f: Symbol<unsafe extern "C" fn() -> *mut core::ffi::c_void> =
                    lib.get(&sym_bytes).map_err(|e| e.to_string())?;
                Ok(Val::Num((f() as usize) as f64))
            }
            (CAbiType::Ptr, [CAbiType::Ptr]) => {
                let f: Symbol<
                    unsafe extern "C" fn(*mut core::ffi::c_void) -> *mut core::ffi::c_void,
                > = lib.get(&sym_bytes).map_err(|e| e.to_string())?;
                let CArg::Ptr(p) = cargs[0] else {
                    unreachable!()
                };
                Ok(Val::Num((f(p) as usize) as f64))
            }
            (CAbiType::CStr, []) => {
                let f: Symbol<unsafe extern "C" fn() -> *const c_char> =
                    lib.get(&sym_bytes).map_err(|e| e.to_string())?;
                let ptr = f();
                Ok(if ptr.is_null() {
                    Val::Null
                } else {
                    Val::Str(CStr::from_ptr(ptr).to_string_lossy().into_owned())
                })
            }
            (CAbiType::I32, [CAbiType::I32]) => {
                let f: Symbol<unsafe extern "C" fn(i32) -> i32> =
                    lib.get(&sym_bytes).map_err(|e| e.to_string())?;
                let CArg::I32(a) = cargs[0] else {
                    unreachable!()
                };
                Ok(Val::Num(f(a) as f64))
            }
            (CAbiType::I32, [CAbiType::I32, CAbiType::I32]) => {
                let f: Symbol<unsafe extern "C" fn(i32, i32) -> i32> =
                    lib.get(&sym_bytes).map_err(|e| e.to_string())?;
                let (CArg::I32(a), CArg::I32(b)) = (&cargs[0], &cargs[1]) else {
                    unreachable!()
                };
                Ok(Val::Num(f(*a, *b) as f64))
            }
            (CAbiType::I32, [CAbiType::I32, CAbiType::I32, CAbiType::I32]) => {
                let f: Symbol<unsafe extern "C" fn(i32, i32, i32) -> i32> =
                    lib.get(&sym_bytes).map_err(|e| e.to_string())?;
                let (CArg::I32(a), CArg::I32(b), CArg::I32(c)) = (&cargs[0], &cargs[1], &cargs[2])
                else {
                    unreachable!()
                };
                Ok(Val::Num(f(*a, *b, *c) as f64))
            }
            (CAbiType::I64, [CAbiType::I64]) => {
                let f: Symbol<unsafe extern "C" fn(i64) -> i64> =
                    lib.get(&sym_bytes).map_err(|e| e.to_string())?;
                let CArg::I64(a) = cargs[0] else {
                    unreachable!()
                };
                Ok(Val::Num(f(a) as f64))
            }
            (CAbiType::I64, [CAbiType::I64, CAbiType::I64]) => {
                let f: Symbol<unsafe extern "C" fn(i64, i64) -> i64> =
                    lib.get(&sym_bytes).map_err(|e| e.to_string())?;
                let (CArg::I64(a), CArg::I64(b)) = (&cargs[0], &cargs[1]) else {
                    unreachable!()
                };
                Ok(Val::Num(f(*a, *b) as f64))
            }
            (CAbiType::U64, [CAbiType::U64, CAbiType::I64]) => {
                let f: Symbol<unsafe extern "C" fn(u64, i64) -> u64> =
                    lib.get(&sym_bytes).map_err(|e| e.to_string())?;
                let (CArg::U64(a), CArg::I64(b)) = (&cargs[0], &cargs[1]) else {
                    unreachable!()
                };
                Ok(Val::Num(f(*a, *b) as f64))
            }
            (CAbiType::F64, [CAbiType::F64]) => {
                let f: Symbol<unsafe extern "C" fn(f64) -> f64> =
                    lib.get(&sym_bytes).map_err(|e| e.to_string())?;
                let CArg::F64(a) = cargs[0] else {
                    unreachable!()
                };
                Ok(Val::Num(f(a)))
            }
            (CAbiType::F64, [CAbiType::F64, CAbiType::F64]) => {
                let f: Symbol<unsafe extern "C" fn(f64, f64) -> f64> =
                    lib.get(&sym_bytes).map_err(|e| e.to_string())?;
                let (CArg::F64(a), CArg::F64(b)) = (&cargs[0], &cargs[1]) else {
                    unreachable!()
                };
                Ok(Val::Num(f(*a, *b)))
            }
            (CAbiType::F64, [CAbiType::F64, CAbiType::F64, CAbiType::F64]) => {
                let f: Symbol<unsafe extern "C" fn(f64, f64, f64) -> f64> =
                    lib.get(&sym_bytes).map_err(|e| e.to_string())?;
                let (CArg::F64(a), CArg::F64(b), CArg::F64(c)) = (&cargs[0], &cargs[1], &cargs[2])
                else {
                    unreachable!()
                };
                Ok(Val::Num(f(*a, *b, *c)))
            }
            (CAbiType::F32, [CAbiType::F32, CAbiType::F32]) => {
                let f: Symbol<unsafe extern "C" fn(f32, f32) -> f32> =
                    lib.get(&sym_bytes).map_err(|e| e.to_string())?;
                let (CArg::F32(a), CArg::F32(b)) = (&cargs[0], &cargs[1]) else {
                    unreachable!()
                };
                Ok(Val::Num(f(*a, *b) as f64))
            }
            (CAbiType::U64, [CAbiType::CStr]) => {
                let f: Symbol<unsafe extern "C" fn(*const c_char) -> u64> =
                    lib.get(&sym_bytes).map_err(|e| e.to_string())?;
                let CArg::CStr(ref s) = cargs[0] else {
                    unreachable!()
                };
                Ok(Val::Num(f(s.as_ptr()) as f64))
            }
            (CAbiType::I32, [CAbiType::CStr]) => {
                let f: Symbol<unsafe extern "C" fn(*const c_char) -> i32> =
                    lib.get(&sym_bytes).map_err(|e| e.to_string())?;
                let CArg::CStr(ref s) = cargs[0] else {
                    unreachable!()
                };
                Ok(Val::Num(f(s.as_ptr()) as f64))
            }
            (CAbiType::I32, [CAbiType::CStr, CAbiType::CStr]) => {
                let f: Symbol<unsafe extern "C" fn(*const c_char, *const c_char) -> i32> =
                    lib.get(&sym_bytes).map_err(|e| e.to_string())?;
                let (CArg::CStr(a), CArg::CStr(b)) = (&cargs[0], &cargs[1]) else {
                    unreachable!()
                };
                Ok(Val::Num(f(a.as_ptr(), b.as_ptr()) as f64))
            }
            (CAbiType::F64, [CAbiType::F64, CAbiType::I32]) => {
                let f: Symbol<unsafe extern "C" fn(f64, i32) -> f64> =
                    lib.get(&sym_bytes).map_err(|e| e.to_string())?;
                let (CArg::F64(a), CArg::I32(b)) = (&cargs[0], &cargs[1]) else {
                    unreachable!()
                };
                Ok(Val::Num(f(*a, *b)))
            }
            _ => Err(format!(
                "ffi_call: signature ({:?}) -> {} is not supported",
                param_tys,
                ret_ty.tag()
            )),
        }
    }
}
