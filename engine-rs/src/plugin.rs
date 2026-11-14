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

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
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

#[derive(Debug, Clone)]
enum CConstVal {
    Int(i64),
    Float(f64),
    Str(String),
}

#[derive(Debug, Clone)]
struct CHeaderConst {
    name: String,
    val: CConstVal,
}

#[derive(Debug, Clone)]
struct CStructField {
    name: String,
    offset: usize,
    read_fn: Option<&'static str>,
    write_fn: Option<&'static str>,
}

#[derive(Debug, Clone)]
struct CStructLayout {
    name: String,
    size: usize,
    align: usize,
    fields: Vec<CStructField>,
}

#[derive(Debug, Default)]
struct CHeaderParseResult {
    consts: Vec<CHeaderConst>,
    structs: Vec<CStructLayout>,
    funcs: Vec<CHeaderFn>,
}

fn parse_c_int_literal(raw: &str) -> Option<i64> {
    let s = raw.trim().trim_end_matches(['u', 'U', 'l', 'L']);
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        i64::from_str_radix(hex, 16).ok()
    } else if let Some(neg_hex) = s.strip_prefix("-0x").or_else(|| s.strip_prefix("-0X")) {
        i64::from_str_radix(neg_hex, 16).ok().map(|v| -v)
    } else {
        s.parse::<i64>().ok()
    }
}

fn split_top_level_decls(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    for ch in src.chars() {
        match ch {
            '{' => {
                depth += 1;
                cur.push(ch);
            }
            '}' => {
                depth = (depth - 1).max(0);
                cur.push(ch);
            }
            ';' if depth == 0 => {
                let trimmed = cur.trim();
                if !trimmed.is_empty() {
                    out.push(trimmed.to_string());
                }
                cur.clear();
            }
            _ => cur.push(ch),
        }
    }
    let rem = cur.trim();
    if !rem.is_empty() {
        out.push(rem.to_string());
    }
    out
}

fn c_field_layout(
    raw_ty: &str,
    typedefs: &HashMap<String, CAbiType>,
    structs: &HashMap<String, CStructLayout>,
) -> (usize, usize, Option<&'static str>, Option<&'static str>) {
    let mut cleaned = raw_ty.trim().replace('*', " * ");
    for qual in ["const", "volatile", "restrict", "struct", "enum", "signed"] {
        cleaned = cleaned
            .split_whitespace()
            .filter(|w| *w != qual)
            .collect::<Vec<_>>()
            .join(" ");
    }
    let norm = cleaned.trim();
    if norm.contains('*') {
        return (8, 8, Some("mem_read_i64"), Some("mem_write_i64"));
    }
    if let Some(st) = structs.get(norm) {
        return (st.size.max(1), st.align.max(1), None, None);
    }
    if let Some(&ty) = typedefs.get(norm) {
        return match ty {
            CAbiType::Bool => (1, 1, Some("mem_read_u8"), Some("mem_write_u8")),
            CAbiType::I32 | CAbiType::U32 => (4, 4, Some("mem_read_i32"), Some("mem_write_i32")),
            CAbiType::F32 => (4, 4, Some("mem_read_f32"), Some("mem_write_f32")),
            CAbiType::F64 => (8, 8, Some("mem_read_f64"), Some("mem_write_f64")),
            _ => (8, 8, Some("mem_read_i64"), Some("mem_write_i64")),
        };
    }
    match norm {
        "char" | "unsigned char" | "int8_t" | "uint8_t" | "bool" | "_Bool" => {
            (1, 1, Some("mem_read_u8"), Some("mem_write_u8"))
        }
        "short" | "short int" | "unsigned short" | "int16_t" | "uint16_t" => (2, 2, None, None),
        "int" | "unsigned" | "unsigned int" | "int32_t" | "uint32_t" => {
            (4, 4, Some("mem_read_i32"), Some("mem_write_i32"))
        }
        "float" => (4, 4, Some("mem_read_f32"), Some("mem_write_f32")),
        "double" | "long double" => (8, 8, Some("mem_read_f64"), Some("mem_write_f64")),
        "long" | "long int" | "unsigned long" | "unsigned long int" => {
            if cfg!(windows) {
                (4, 4, Some("mem_read_i32"), Some("mem_write_i32"))
            } else {
                (8, 8, Some("mem_read_i64"), Some("mem_write_i64"))
            }
        }
        _ => (8, 8, Some("mem_read_i64"), Some("mem_write_i64")),
    }
}

fn parse_c_enum_body(body: &str, out: &mut Vec<CHeaderConst>) {
    let mut next_val: i64 = 0;
    for entry in body.split(',') {
        let e = entry.trim();
        if e.is_empty() {
            continue;
        }
        if let Some((lhs, rhs)) = e.split_once('=') {
            let name = lhs.trim();
            if let Some(v) = parse_c_int_literal(rhs) {
                next_val = v;
            }
            if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                out.push(CHeaderConst {
                    name: name.to_string(),
                    val: CConstVal::Int(next_val),
                });
                next_val += 1;
            }
        } else if e.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            out.push(CHeaderConst {
                name: e.to_string(),
                val: CConstVal::Int(next_val),
            });
            next_val += 1;
        }
    }
}

fn parse_c_struct_layout(
    name: &str,
    body: &str,
    typedefs: &HashMap<String, CAbiType>,
    structs: &HashMap<String, CStructLayout>,
) -> CStructLayout {
    let mut offset: usize = 0;
    let mut max_align: usize = 1;
    let mut fields = Vec::new();
    for raw_f in body.split(';') {
        let f = raw_f.trim();
        if f.is_empty() || f.contains('{') || f.contains('}') || f.contains('(') {
            continue;
        }
        let mut array_len = 1usize;
        let base_decl = if let Some(b_open) = f.rfind('[')
            && let Some(b_close) = f.rfind(']')
            && b_open < b_close
        {
            if let Ok(n) = f[b_open + 1..b_close].trim().parse::<usize>() {
                array_len = n.max(1);
            }
            f[..b_open].trim()
        } else {
            f
        };
        let (fname, fty_raw) = if let Some(last_star) = base_decl.rfind('*') {
            let after = base_decl[last_star + 1..].trim();
            let clean: String = after
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            (clean, format!("{} *", base_decl[..last_star].trim()))
        } else {
            let parts: Vec<&str> = base_decl.split_whitespace().collect();
            if parts.len() < 2 {
                continue;
            }
            let cname = parts[parts.len() - 1]
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect::<String>();
            (cname, parts[..parts.len() - 1].join(" "))
        };
        if fname.is_empty() {
            continue;
        }
        let (elem_sz, align, rf, wf) = c_field_layout(&fty_raw, typedefs, structs);
        max_align = max_align.max(align);
        offset = (offset + align - 1) & !(align - 1);
        fields.push(CStructField {
            name: fname,
            offset,
            read_fn: if array_len == 1 { rf } else { None },
            write_fn: if array_len == 1 { wf } else { None },
        });
        offset += elem_sz.saturating_mul(array_len);
    }
    let size = ((offset + max_align - 1) & !(max_align - 1)).max(1);
    CStructLayout {
        name: name.to_string(),
        size,
        align: max_align,
        fields,
    }
}

fn parse_c_header_full(header_src: &str) -> CHeaderParseResult {
    let stripped = strip_c_comments(header_src);
    let mut res = CHeaderParseResult::default();
    let mut logical = String::new();
    for line in stripped.lines() {
        let t = line.trim();
        if let Some(def_rest) = t.strip_prefix("#define ") {
            let rest = def_rest.trim();
            let mut split_it = rest.splitn(2, char::is_whitespace);
            if let (Some(name), Some(val_raw)) = (split_it.next(), split_it.next()) {
                let v = val_raw.trim();
                if !name.contains('(')
                    && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                {
                    if v.starts_with('"') && v.ends_with('"') && v.len() >= 2 {
                        res.consts.push(CHeaderConst {
                            name: name.to_string(),
                            val: CConstVal::Str(v[1..v.len() - 1].to_string()),
                        });
                    } else if let Some(iv) = parse_c_int_literal(v) {
                        res.consts.push(CHeaderConst {
                            name: name.to_string(),
                            val: CConstVal::Int(iv),
                        });
                    } else if let Ok(fv) = v.trim_end_matches(['f', 'F']).parse::<f64>() {
                        res.consts.push(CHeaderConst {
                            name: name.to_string(),
                            val: CConstVal::Float(fv),
                        });
                    }
                }
            }
            continue;
        }
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
    let mut struct_map: HashMap<String, CStructLayout> = HashMap::new();

    for stmt in split_top_level_decls(&logical) {
        if let Some(b_open) = stmt.find('{')
            && let Some(b_close) = stmt.rfind('}')
            && b_open < b_close
        {
            let head = stmt[..b_open].trim();
            let body = &stmt[b_open + 1..b_close];
            let tail = stmt[b_close + 1..].trim();
            if head.starts_with("enum ") || head == "enum" || head.starts_with("typedef enum") {
                parse_c_enum_body(body, &mut res.consts);
                if let Some(tag) = head
                    .strip_prefix("typedef enum")
                    .or_else(|| head.strip_prefix("enum"))
                    .map(str::trim)
                    && !tag.is_empty()
                {
                    typedefs.insert(tag.to_string(), CAbiType::I32);
                }
                if !tail.is_empty() {
                    typedefs.insert(tail.to_string(), CAbiType::I32);
                }
                continue;
            }
            if head.starts_with("struct ") || head == "struct" || head.starts_with("typedef struct")
            {
                let name = if !tail.is_empty() {
                    tail.to_string()
                } else if let Some(tag) = head
                    .strip_prefix("typedef struct")
                    .or_else(|| head.strip_prefix("struct"))
                    .map(str::trim)
                    && !tag.is_empty()
                {
                    tag.to_string()
                } else {
                    continue;
                };
                let layout = parse_c_struct_layout(&name, body, &typedefs, &struct_map);
                struct_map.insert(name, layout.clone());
                res.structs.push(layout);
                continue;
            }
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
        res.funcs.push(CHeaderFn {
            name: fn_name.to_string(),
            ret,
            params,
        });
    }
    res
}

pub(crate) fn ffi_bindgen(header_src: &str, lib_path: &str) -> Result<String, String> {
    let parsed = parse_c_header_full(header_src);
    if parsed.funcs.is_empty() && parsed.consts.is_empty() && parsed.structs.is_empty() {
        return Err("ffi_bindgen found no C declarations in header".into());
    }
    let escaped_lib = lib_path.replace('\\', "\\\\").replace('"', "\\\"");
    let mut out = String::new();
    out.push_str("// Auto-generated by veyra ffi bindgen\n");
    out.push_str(&format!("let __FFI_LIB = \"{escaped_lib}\";\n\n"));
    for c in &parsed.consts {
        match &c.val {
            CConstVal::Int(i) => out.push_str(&format!("let {} = {i};\n", c.name)),
            CConstVal::Float(f) => out.push_str(&format!("let {} = {f};\n", c.name)),
            CConstVal::Str(s) => {
                let esc = s.replace('\\', "\\\\").replace('"', "\\\"");
                out.push_str(&format!("let {} = \"{esc}\";\n", c.name));
            }
        }
    }
    if !parsed.consts.is_empty() {
        out.push('\n');
    }
    for st in &parsed.structs {
        out.push_str(&format!("let {}_SIZE = {};\n", st.name, st.size));
        out.push_str(&format!("let {}_ALIGN = {};\n", st.name, st.align));
        for f in &st.fields {
            out.push_str(&format!(
                "let {}_{}_OFFSET = {};\n",
                st.name, f.name, f.offset
            ));
        }
        out.push_str(&format!(
            "fn {}_alloc() -> Int {{ return mem_alloc({}_SIZE); }}\n",
            st.name, st.name
        ));
        let pack_fields: Vec<&CStructField> =
            st.fields.iter().filter(|f| f.write_fn.is_some()).collect();
        let pack_params = pack_fields
            .iter()
            .map(|f| f.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let sig_comma = if pack_params.is_empty() { "" } else { ", " };
        out.push_str(&format!(
            "fn {}_pack(ptr: Int{sig_comma}{pack_params}) -> Int {{\n",
            st.name
        ));
        for f in &pack_fields {
            if let Some(wf) = f.write_fn {
                out.push_str(&format!("    {wf}(ptr, {}, {});\n", f.offset, f.name));
            }
        }
        out.push_str("    return ptr;\n}\n");
        let unpack_fields: Vec<&CStructField> =
            st.fields.iter().filter(|f| f.read_fn.is_some()).collect();
        out.push_str(&format!(
            "fn {}_unpack(ptr: Int) -> Map {{\n    return {{",
            st.name
        ));
        for (idx, f) in unpack_fields.iter().enumerate() {
            if idx > 0 {
                out.push_str(", ");
            }
            if let Some(rf) = f.read_fn {
                out.push_str(&format!("\"{}\": {rf}(ptr, {})", f.name, f.offset));
            }
        }
        out.push_str("};\n}\n\n");
    }
    for f in parsed.funcs {
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

static FFI_ALLOCS: OnceLock<Mutex<HashMap<usize, Box<[u8]>>>> = OnceLock::new();

fn ffi_allocs() -> &'static Mutex<HashMap<usize, Box<[u8]>>> {
    FFI_ALLOCS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn mem_alloc(size: usize) -> Result<usize, String> {
    if size == 0 || size > 256 * 1024 * 1024 {
        return Err("mem_alloc size must be between 1 and 268435456 bytes".into());
    }
    let mut buf = vec![0u8; size].into_boxed_slice();
    let addr = buf.as_mut_ptr() as usize;
    let mut guard = ffi_allocs()
        .lock()
        .map_err(|_| "ffi alloc table poisoned")?;
    guard.insert(addr, buf);
    Ok(addr)
}

pub(crate) fn mem_free(ptr: usize) -> Result<bool, String> {
    let mut guard = ffi_allocs()
        .lock()
        .map_err(|_| "ffi alloc table poisoned")?;
    if guard.remove(&ptr).is_some() {
        Ok(true)
    } else {
        Err(format!(
            "mem_free: invalid or already-freed pointer 0x{ptr:x}"
        ))
    }
}

fn with_tracked_bytes<R>(
    ptr: usize,
    offset: usize,
    width: usize,
    f: impl FnOnce(&mut [u8]) -> R,
) -> Result<R, String> {
    if ptr == 0 {
        return Err("null pointer dereference".into());
    }
    let end = ptr
        .checked_add(offset)
        .and_then(|p| p.checked_add(width))
        .ok_or_else(|| "pointer offset overflow".to_string())?;
    let mut guard = ffi_allocs()
        .lock()
        .map_err(|_| "ffi alloc table poisoned")?;
    for (&base, buf) in guard.iter_mut() {
        let buf_end = base.saturating_add(buf.len());
        if ptr >= base && end <= buf_end {
            let rel = (ptr - base) + offset;
            return Ok(f(&mut buf[rel..rel + width]));
        }
    }
    Err(format!(
        "memory bounds error: pointer 0x{ptr:x} (offset {offset}, width {width}) outside live FFI allocations"
    ))
}

pub(crate) fn mem_read_u8(ptr: usize, offset: usize) -> Result<u8, String> {
    with_tracked_bytes(ptr, offset, 1, |s| s[0])
}
pub(crate) fn mem_read_i32(ptr: usize, offset: usize) -> Result<i32, String> {
    with_tracked_bytes(ptr, offset, 4, |s| {
        i32::from_ne_bytes(s.try_into().unwrap())
    })
}
pub(crate) fn mem_read_i64(ptr: usize, offset: usize) -> Result<i64, String> {
    with_tracked_bytes(ptr, offset, 8, |s| {
        i64::from_ne_bytes(s.try_into().unwrap())
    })
}
pub(crate) fn mem_read_f32(ptr: usize, offset: usize) -> Result<f32, String> {
    with_tracked_bytes(ptr, offset, 4, |s| {
        f32::from_ne_bytes(s.try_into().unwrap())
    })
}
pub(crate) fn mem_read_f64(ptr: usize, offset: usize) -> Result<f64, String> {
    with_tracked_bytes(ptr, offset, 8, |s| {
        f64::from_ne_bytes(s.try_into().unwrap())
    })
}
pub(crate) fn mem_write_u8(ptr: usize, offset: usize, val: u8) -> Result<(), String> {
    with_tracked_bytes(ptr, offset, 1, |s| s[0] = val)
}
pub(crate) fn mem_write_i32(ptr: usize, offset: usize, val: i32) -> Result<(), String> {
    with_tracked_bytes(ptr, offset, 4, |s| s.copy_from_slice(&val.to_ne_bytes()))
}
pub(crate) fn mem_write_i64(ptr: usize, offset: usize, val: i64) -> Result<(), String> {
    with_tracked_bytes(ptr, offset, 8, |s| s.copy_from_slice(&val.to_ne_bytes()))
}
pub(crate) fn mem_write_f32(ptr: usize, offset: usize, val: f32) -> Result<(), String> {
    with_tracked_bytes(ptr, offset, 4, |s| s.copy_from_slice(&val.to_ne_bytes()))
}
pub(crate) fn mem_write_f64(ptr: usize, offset: usize, val: f64) -> Result<(), String> {
    with_tracked_bytes(ptr, offset, 8, |s| s.copy_from_slice(&val.to_ne_bytes()))
}
pub(crate) fn mem_read_cstr(ptr: usize, offset: usize) -> Result<String, String> {
    if ptr == 0 {
        return Err("null pointer dereference".into());
    }
    let start = ptr
        .checked_add(offset)
        .ok_or_else(|| "pointer offset overflow".to_string())?;
    {
        let guard = ffi_allocs()
            .lock()
            .map_err(|_| "ffi alloc table poisoned")?;
        for (&base, buf) in guard.iter() {
            let buf_end = base.saturating_add(buf.len());
            if start >= base && start < buf_end {
                let rel = start - base;
                let tail = &buf[rel..];
                let nul = tail.iter().position(|&b| b == 0).unwrap_or(tail.len());
                return Ok(String::from_utf8_lossy(&tail[..nul]).into_owned());
            }
        }
    }
    // Safety: caller passed a C string pointer returned by native FFI code.
    let s = unsafe { CStr::from_ptr(start as *const c_char) };
    Ok(s.to_string_lossy().into_owned())
}

#[cfg(target_arch = "x86_64")]
#[repr(C)]
struct AbiFrame {
    fn_ptr: u64,
    args_u64: [u64; 16],
    ret_gp: u64,
    ret_xmm: u64,
}

#[cfg(target_arch = "x86_64")]
static DYNAMIC_FFI_STUBS: OnceLock<Mutex<HashMap<Vec<CAbiType>, Arc<crate::jit::ExecMem>>>> =
    OnceLock::new();

#[cfg(target_arch = "x86_64")]
fn emit_mov_gp_from_rbx(code: &mut Vec<u8>, gp_reg: u8, disp32: i32) {
    let rex = 0x48 | if gp_reg >= 8 { 0x04 } else { 0 };
    code.push(rex);
    code.push(0x8B);
    code.push(0x83 | ((gp_reg & 7) << 3));
    code.extend_from_slice(&disp32.to_le_bytes());
}

#[cfg(target_arch = "x86_64")]
fn emit_movsd_xmm_from_rbx(code: &mut Vec<u8>, xmm_reg: u8, disp32: i32) {
    code.extend_from_slice(&[0xF2, 0x0F, 0x10, 0x83 | ((xmm_reg & 7) << 3)]);
    code.extend_from_slice(&disp32.to_le_bytes());
}

#[cfg(target_arch = "x86_64")]
fn get_or_build_ffi_trampoline(param_tys: &[CAbiType]) -> Result<Arc<crate::jit::ExecMem>, String> {
    let stubs = DYNAMIC_FFI_STUBS.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(guard) = stubs.lock()
        && let Some(cached) = guard.get(param_tys)
    {
        return Ok(cached.clone());
    }

    let mut code = Vec::with_capacity(256);
    // Prologue: push rbp; mov rbp, rsp; push rbx; sub rsp, 136 (keeps RSP 16-byte aligned before call)
    code.extend_from_slice(&[0x55, 0x48, 0x89, 0xE5, 0x53, 0x48, 0x81, 0xEC, 136, 0, 0, 0]);

    #[cfg(windows)]
    {
        // mov rbx, rcx (AbiFrame pointer)
        code.extend_from_slice(&[0x48, 0x89, 0xCB]);
        const WIN_GP_REGS: [u8; 4] = [1, 2, 8, 9]; // rcx, rdx, r8, r9
        for (i, ty) in param_tys.iter().enumerate() {
            let disp = (8 + i * 8) as i32;
            let is_fp = matches!(ty, CAbiType::F32 | CAbiType::F64);
            if i < 4 {
                if is_fp {
                    emit_movsd_xmm_from_rbx(&mut code, i as u8, disp);
                } else {
                    emit_mov_gp_from_rbx(&mut code, WIN_GP_REGS[i], disp);
                }
            } else {
                // mov rax, [rbx + disp]; mov [rsp + 32 + (i - 4) * 8], rax
                emit_mov_gp_from_rbx(&mut code, 0, disp);
                let stack_off = (32 + (i - 4) * 8) as i32;
                code.extend_from_slice(&[0x48, 0x89, 0x84, 0x24]);
                code.extend_from_slice(&stack_off.to_le_bytes());
            }
        }
        // mov rax, [rbx]; call rax
        code.extend_from_slice(&[0x48, 0x8B, 0x03, 0xFF, 0xD0]);
    }

    #[cfg(unix)]
    {
        // mov rbx, rdi (AbiFrame pointer)
        code.extend_from_slice(&[0x48, 0x89, 0xFB]);
        const SYSV_GP_REGS: [u8; 6] = [7, 6, 2, 1, 8, 9]; // rdi, rsi, rdx, rcx, r8, r9
        let mut gp_idx = 0usize;
        let mut xmm_idx = 0usize;
        let mut stack_off = 0i32;
        for (i, ty) in param_tys.iter().enumerate() {
            let disp = (8 + i * 8) as i32;
            let is_fp = matches!(ty, CAbiType::F32 | CAbiType::F64);
            if is_fp && xmm_idx < 8 {
                emit_movsd_xmm_from_rbx(&mut code, xmm_idx as u8, disp);
                xmm_idx += 1;
            } else if !is_fp && gp_idx < 6 {
                emit_mov_gp_from_rbx(&mut code, SYSV_GP_REGS[gp_idx], disp);
                gp_idx += 1;
            } else {
                emit_mov_gp_from_rbx(&mut code, 0, disp);
                code.extend_from_slice(&[0x48, 0x89, 0x84, 0x24]);
                code.extend_from_slice(&stack_off.to_le_bytes());
                stack_off += 8;
            }
        }
        // mov eax, xmm_idx; mov r11, [rbx]; call r11
        code.extend_from_slice(&[
            0xB8,
            (xmm_idx.min(8)) as u8,
            0,
            0,
            0,
            0x4C,
            0x8B,
            0x1B,
            0x41,
            0xFF,
            0xD3,
        ]);
    }

    // Capture returns: mov [rbx + 136], rax; movsd [rbx + 144], xmm0
    code.extend_from_slice(&[0x48, 0x89, 0x83]);
    code.extend_from_slice(&136i32.to_le_bytes());
    code.extend_from_slice(&[0xF2, 0x0F, 0x11, 0x83]);
    code.extend_from_slice(&144i32.to_le_bytes());
    // Epilogue: add rsp, 136; pop rbx; pop rbp; ret
    code.extend_from_slice(&[0x48, 0x81, 0xC4, 136, 0, 0, 0, 0x5B, 0x5D, 0xC3]);

    let exec = Arc::new(crate::jit::alloc_executable(&code)?);
    if let Ok(mut guard) = stubs.lock() {
        guard.insert(param_tys.to_vec(), exec.clone());
    }
    Ok(exec)
}

#[cfg(target_arch = "x86_64")]
fn invoke_c_abi_dynamic(
    fn_ptr: usize,
    ret_ty: CAbiType,
    param_tys: &[CAbiType],
    cargs: &[CArg],
) -> Result<Val, String> {
    if param_tys.len() > 16 {
        return Err("ffi_call supports up to 16 arguments dynamically".into());
    }
    let stub = get_or_build_ffi_trampoline(param_tys)?;
    let mut frame = AbiFrame {
        fn_ptr: fn_ptr as u64,
        args_u64: [0; 16],
        ret_gp: 0,
        ret_xmm: 0,
    };
    for (i, a) in cargs.iter().enumerate() {
        frame.args_u64[i] = match a {
            CArg::I32(v) => (*v as i64) as u64,
            CArg::U32(v) => *v as u64,
            CArg::I64(v) => *v as u64,
            CArg::U64(v) => *v,
            CArg::Bool(b) => u64::from(*b),
            CArg::F32(f) => f.to_bits() as u64,
            CArg::F64(f) => f.to_bits(),
            CArg::Ptr(p) => *p as usize as u64,
            CArg::CStr(s) => s.as_ptr() as usize as u64,
        };
    }
    unsafe {
        let entry: unsafe extern "C" fn(*mut AbiFrame) = core::mem::transmute(stub.ptr);
        entry(&mut frame);
    }
    Ok(match ret_ty {
        CAbiType::Void => Val::Null,
        CAbiType::I32 => Val::Num((frame.ret_gp as u32 as i32) as f64),
        CAbiType::U32 => Val::Num((frame.ret_gp as u32) as f64),
        CAbiType::I64 => Val::Num((frame.ret_gp as i64) as f64),
        CAbiType::U64 => Val::Num(frame.ret_gp as f64),
        CAbiType::Bool => Val::Bool((frame.ret_gp as u8) != 0),
        CAbiType::Ptr => Val::Num((frame.ret_gp as usize) as f64),
        CAbiType::F32 => Val::Num(f32::from_bits(frame.ret_xmm as u32) as f64),
        CAbiType::F64 => Val::Num(f64::from_bits(frame.ret_xmm)),
        CAbiType::CStr => {
            let p = frame.ret_gp as usize as *const c_char;
            if p.is_null() {
                Val::Null
            } else {
                Val::Str(unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
            }
        }
    })
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

    #[cfg(target_arch = "x86_64")]
    {
        let raw_sym: Symbol<unsafe extern "C" fn()> =
            unsafe { lib.get(&sym_bytes) }.map_err(|e| e.to_string())?;
        let fn_ptr = (*raw_sym) as usize;
        invoke_c_abi_dynamic(fn_ptr, ret_ty, &param_tys, &cargs)
    }

    #[cfg(not(target_arch = "x86_64"))]
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
