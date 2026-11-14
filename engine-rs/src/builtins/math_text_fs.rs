#![allow(
    clippy::collapsible_if,
    clippy::needless_range_loop,
    clippy::type_complexity,
    clippy::let_and_return,
    clippy::only_used_in_recursion,
    unused_imports
)]

use std::{
    collections::{HashMap, HashSet},
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::Ordering,
        mpsc::{self, TryRecvError},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::Engine;
use libloading::Library;
use rand::Rng;
use regex::Regex;
#[cfg(feature = "native-services")]
use rusqlite::Connection;
use serde_json::Value as JsonValue;
use sha2::{Digest, Sha256};

use crate::bigint::{BigArg, BigOut, eval_bigint_builtin, is_bigint_builtin};
use crate::bytecode::{ClassDef, FunctionDef, ObjectData, Op};
use crate::pipeline::{compile_source, load_source};
use crate::project::project_root;
use crate::runtime::{
    CHANNEL_IDS, FUNCTION_IDS, GENERATOR_IDS, OBJECT_IDS, TASK_IDS, channels,
    export_function_object, namespace_module_ops, register_named_function_object,
    resolve_function_object, tasks,
};
use crate::tensor::{
    AdamState, TensorOp, adam_states, tensor_backward, tensor_from_value, tensor_id,
    tensor_snapshot, tensor_store, tensors,
};
use crate::value::{
    Val, as_matrix, as_num_list, constant_time_strings_equal, distance_sq, hmac_sha256_hex,
    json_to_val, map_lookup, matrix_determinant, matrix_inverse, matrix_to_val, nums_to_val,
    pbkdf2_sha256_hex, solve_linear_system, svg_escape, val_to_json,
};
#[cfg(feature = "native-services")]
use crate::value::{json_stringify_value, val_to_sql_value};
use crate::vm::{Vm, collect_generator, generators, resume_generator};

fn val_to_big_arg(v: &Val) -> BigArg {
    match v {
        Val::Str(s) => BigArg::Str(s.clone()),
        Val::Num(n) => BigArg::Num(*n),
        Val::List(xs) => BigArg::List(xs.iter().map(val_to_big_arg).collect()),
        Val::Map(pairs) => {
            let mut re = None;
            let mut im = 0.0;
            for (k, val) in pairs {
                if let (Val::Str(ks), Val::Num(x)) = (k, val) {
                    if ks == "re" {
                        re = Some(*x);
                    } else if ks == "im" {
                        im = *x;
                    }
                }
            }
            match re {
                Some(r) => BigArg::Complex(r, im),
                None => BigArg::Other,
            }
        }
        _ => BigArg::Other,
    }
}

fn big_out_to_val(out: BigOut) -> Val {
    match out {
        BigOut::Str(s) => Val::Str(s),
        BigOut::Num(n) => Val::Num(n),
        BigOut::StrPair(q, r) => Val::List(vec![Val::Str(q), Val::Str(r)]),
        BigOut::Complex(r, i) => Val::List(vec![Val::Num(r), Val::Num(i)]),
        BigOut::ComplexList(zs) => Val::List(
            zs.into_iter()
                .map(|(r, i)| Val::List(vec![Val::Num(r), Val::Num(i)]))
                .collect(),
        ),
    }
}

impl Vm {
    pub(crate) fn call_builtin_math_text_fs(
        &mut self,
        n: &str,
        a: Vec<Val>,
    ) -> Result<Option<Val>, String> {
        if is_bigint_builtin(n) {
            let bargs: Vec<BigArg> = a.iter().map(val_to_big_arg).collect();
            return Ok(eval_bigint_builtin(n, &bargs)?.map(big_out_to_val));
        }
        let v = match n {
            "json_stringify" => {
                if a.len() != 1 {
                    return Err("json_stringify expects 1 argument".into());
                }
                Val::Str(serde_json::to_string(&val_to_json(&a[0])).map_err(|e| e.to_string())?)
            }
            "json_parse" => {
                if a.len() != 1 {
                    return Err("json_parse expects 1 argument".into());
                }
                let s = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("json_parse expects String".into()),
                };
                json_to_val(&serde_json::from_str::<JsonValue>(s).map_err(|e| e.to_string())?)
            }
            "regex_match" => {
                if a.len() != 2 {
                    return Err("regex_match expects pattern,text".into());
                }
                let p = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("pattern must be String".into()),
                };
                let t = match &a[1] {
                    Val::Str(s) => s,
                    _ => return Err("text must be String".into()),
                };
                Val::Bool(Regex::new(p).map_err(|e| e.to_string())?.is_match(t))
            }
            "regex_replace" => {
                if a.len() != 3 {
                    return Err("regex_replace expects pattern,text,replacement".into());
                }
                let p = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("pattern must be String".into()),
                };
                let t = match &a[1] {
                    Val::Str(s) => s,
                    _ => return Err("text must be String".into()),
                };
                let r = match &a[2] {
                    Val::Str(s) => s,
                    _ => return Err("replacement must be String".into()),
                };
                Val::Str(
                    Regex::new(p)
                        .map_err(|e| e.to_string())?
                        .replace_all(t, r.as_str())
                        .to_string(),
                )
            }
            "regex_split" => {
                if a.len() != 2 {
                    return Err("regex_split expects pattern,text".into());
                }
                let p = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("pattern must be String".into()),
                };
                let t = match &a[1] {
                    Val::Str(s) => s,
                    _ => return Err("text must be String".into()),
                };
                Val::List(
                    Regex::new(p)
                        .map_err(|e| e.to_string())?
                        .split(t)
                        .map(|x| Val::Str(x.to_string()))
                        .collect(),
                )
            }
            "regex_findall" => {
                if a.len() != 2 {
                    return Err("regex_findall expects pattern,text".into());
                }
                let p = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("pattern must be String".into()),
                };
                let t = match &a[1] {
                    Val::Str(s) => s,
                    _ => return Err("text must be String".into()),
                };
                let re = Regex::new(p).map_err(|e| e.to_string())?;
                Val::List(
                    re.find_iter(t)
                        .map(|m| Val::Str(m.as_str().to_string()))
                        .collect(),
                )
            }
            "sin" => {
                if a.len() != 1 {
                    return Err("sin expects number".into());
                }
                match a[0] {
                    Val::Num(n) => Val::Num(n.sin()),
                    _ => return Err("sin expects Number".into()),
                }
            }
            "cos" => {
                if a.len() != 1 {
                    return Err("cos expects number".into());
                }
                match a[0] {
                    Val::Num(n) => Val::Num(n.cos()),
                    _ => return Err("cos expects Number".into()),
                }
            }
            "tan" => {
                if a.len() != 1 {
                    return Err("tan expects number".into());
                }
                match a[0] {
                    Val::Num(n) => Val::Num(n.tan()),
                    _ => return Err("tan expects Number".into()),
                }
            }
            "exp" => {
                if a.len() != 1 {
                    return Err("exp expects number".into());
                }
                match a[0] {
                    Val::Num(n) => Val::Num(n.exp()),
                    _ => return Err("exp expects Number".into()),
                }
            }
            "ln" => {
                if a.len() != 1 {
                    return Err("ln expects number".into());
                }
                match a[0] {
                    Val::Num(n) => Val::Num(n.ln()),
                    _ => return Err("ln expects Number".into()),
                }
            }
            "log10" => {
                if a.len() != 1 {
                    return Err("log10 expects number".into());
                }
                match a[0] {
                    Val::Num(n) => Val::Num(n.log10()),
                    _ => return Err("log10 expects Number".into()),
                }
            }
            "log2" => {
                if a.len() != 1 {
                    return Err("log2 expects number".into());
                }
                match a[0] {
                    Val::Num(n) => Val::Num(n.log2()),
                    _ => return Err("log2 expects Number".into()),
                }
            }
            "hypot" => {
                if a.len() != 2 {
                    return Err("hypot expects x,y".into());
                }
                match (&a[0], &a[1]) {
                    (Val::Num(x), Val::Num(y)) => Val::Num(x.hypot(*y)),
                    _ => return Err("hypot expects Numbers".into()),
                }
            }
            "fmod" => {
                if a.len() != 2 {
                    return Err("fmod expects x,y".into());
                }
                match (&a[0], &a[1]) {
                    (Val::Num(x), Val::Num(y)) => Val::Num(x % y),
                    _ => return Err("fmod expects Numbers".into()),
                }
            }
            "atan2" => {
                if a.len() != 2 {
                    return Err("atan2 expects y,x".into());
                }
                match (&a[0], &a[1]) {
                    (Val::Num(y), Val::Num(x)) => Val::Num(y.atan2(*x)),
                    _ => return Err("atan2 expects Numbers".into()),
                }
            }
            "pow" => {
                if a.len() < 2 || a.len() > 3 {
                    return Err("pow expects base,exponent[,modulus]".into());
                }
                match (a[0].clone(), a[1].clone(), a.get(2).cloned()) {
                    (Val::Num(x), Val::Num(y), None) => Val::Num(x.powf(y)),
                    (Val::Num(x), Val::Num(y), Some(Val::Num(m))) => {
                        if m == 0.0 {
                            return Err("pow modulus cannot be zero".into());
                        }
                        Val::Num(x.powf(y) % m)
                    }
                    _ => return Err("pow expects Numbers".into()),
                }
            }
            "round" => {
                if a.is_empty() || a.len() > 2 {
                    return Err("round expects number[, ndigits]".into());
                }
                let n = match a[0] {
                    Val::Num(n) => n,
                    _ => return Err("round expects Number".into()),
                };
                match a.get(1) {
                    None => Val::Num(n.round()),
                    Some(Val::Num(d)) => {
                        let nd = *d as i32;
                        if nd <= 0 {
                            let scale = 10f64.powi(-nd);
                            Val::Num((n / scale).round() * scale)
                        } else {
                            let scale = 10f64.powi(nd);
                            Val::Num((n * scale).round() / scale)
                        }
                    }
                    _ => return Err("round ndigits must be Number".into()),
                }
            }
            "sqrt" => {
                if a.len() != 1 {
                    return Err("sqrt expects 1 argument".into());
                }
                match a[0] {
                    Val::Num(n) => Val::Num(n.sqrt()),
                    _ => return Err("sqrt expects Number".into()),
                }
            }
            "abs" => {
                if a.len() != 1 {
                    return Err("abs expects 1 argument".into());
                }
                match a[0] {
                    Val::Num(n) => Val::Num(n.abs()),
                    _ => return Err("abs expects Number".into()),
                }
            }
            "floor" => {
                if a.len() != 1 {
                    return Err("floor expects 1 argument".into());
                }
                match a[0] {
                    Val::Num(n) => Val::Num(n.floor()),
                    _ => return Err("floor expects Number".into()),
                }
            }
            "ceil" => {
                if a.len() != 1 {
                    return Err("ceil expects 1 argument".into());
                }
                match a[0] {
                    Val::Num(n) => Val::Num(n.ceil()),
                    _ => return Err("ceil expects Number".into()),
                }
            }
            "starts_with" => {
                if a.len() != 2 {
                    return Err("starts_with expects text,prefix".into());
                }
                let text = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("text must be String".into()),
                };
                let prefix = match &a[1] {
                    Val::Str(s) => s,
                    _ => return Err("prefix must be String".into()),
                };
                Val::Bool(text.starts_with(prefix))
            }
            "ends_with" => {
                if a.len() != 2 {
                    return Err("ends_with expects text,suffix".into());
                }
                let text = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("text must be String".into()),
                };
                let suffix = match &a[1] {
                    Val::Str(s) => s,
                    _ => return Err("suffix must be String".into()),
                };
                Val::Bool(text.ends_with(suffix))
            }
            "replace" => {
                if a.len() != 3 {
                    return Err("replace expects text,from,to".into());
                }
                let text = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("text must be String".into()),
                };
                let from = match &a[1] {
                    Val::Str(s) => s,
                    _ => return Err("from must be String".into()),
                };
                let to = match &a[2] {
                    Val::Str(s) => s,
                    _ => return Err("to must be String".into()),
                };
                Val::Str(text.replace(from, to))
            }
            "repeat" => {
                if a.len() != 2 {
                    return Err("repeat expects text,count".into());
                }
                let text = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("text must be String".into()),
                };
                let count = match a[1] {
                    Val::Num(n) => n.max(0.0) as usize,
                    _ => return Err("count must be Number".into()),
                };
                Val::Str(text.repeat(count))
            }
            "chars" => {
                if a.len() != 1 {
                    return Err("chars expects text".into());
                }
                let text = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("text must be String".into()),
                };
                Val::List(text.chars().map(|c| Val::Str(c.to_string())).collect())
            }
            "lower" => {
                if a.len() != 1 {
                    return Err("lower expects String".into());
                }
                match &a[0] {
                    Val::Str(s) => Val::Str(s.to_lowercase()),
                    _ => return Err("lower expects String".into()),
                }
            }
            "upper" => {
                if a.len() != 1 {
                    return Err("upper expects String".into());
                }
                match &a[0] {
                    Val::Str(s) => Val::Str(s.to_uppercase()),
                    _ => return Err("upper expects String".into()),
                }
            }
            "trim" => {
                if a.len() != 1 {
                    return Err("trim expects String".into());
                }
                match &a[0] {
                    Val::Str(s) => Val::Str(s.trim().to_string()),
                    _ => return Err("trim expects String".into()),
                }
            }
            "split" => {
                if a.len() != 2 {
                    return Err("split expects text,separator".into());
                }
                let text = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("text must be String".into()),
                };
                let sep = match &a[1] {
                    Val::Str(s) => s,
                    _ => return Err("separator must be String".into()),
                };
                Val::List(text.split(sep).map(|x| Val::Str(x.to_string())).collect())
            }
            "join" => {
                if a.len() != 2 {
                    return Err("join expects list,separator".into());
                }
                let xs = match &a[0] {
                    Val::List(xs) => xs,
                    _ => return Err("first argument must be List".into()),
                };
                let sep = match &a[1] {
                    Val::Str(s) => s,
                    _ => return Err("separator must be String".into()),
                };
                Val::Str(xs.iter().map(Val::show).collect::<Vec<_>>().join(sep))
            }
            "replace_text" => {
                if a.len() != 3 {
                    return Err("replace_text expects text,from,to".into());
                }
                let text = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("text must be String".into()),
                };
                let from = match &a[1] {
                    Val::Str(s) => s,
                    _ => return Err("from must be String".into()),
                };
                let to = match &a[2] {
                    Val::Str(s) => s,
                    _ => return Err("to must be String".into()),
                };
                Val::Str(text.replace(from, to))
            }
            "path_join" => {
                if a.is_empty() {
                    return Err("path_join expects at least one part".into());
                }
                let mut p = PathBuf::new();
                for part in &a {
                    match part {
                        Val::Str(s) => p.push(s),
                        _ => return Err("path parts must be Strings".into()),
                    }
                }
                Val::Str(p.display().to_string())
            }
            "path_name" => {
                if a.len() != 1 {
                    return Err("path_name expects path".into());
                }
                let p = match &a[0] {
                    Val::Str(s) => Path::new(s),
                    _ => return Err("path must be String".into()),
                };
                Val::Str(
                    p.file_name()
                        .map(|x| x.to_string_lossy().to_string())
                        .unwrap_or_default(),
                )
            }
            "path_parent" => {
                if a.len() != 1 {
                    return Err("path_parent expects path".into());
                }
                let p = match &a[0] {
                    Val::Str(s) => Path::new(s),
                    _ => return Err("path must be String".into()),
                };
                p.parent()
                    .map(|x| Val::Str(x.display().to_string()))
                    .unwrap_or(Val::Null)
            }
            "path_extension" => {
                if a.len() != 1 {
                    return Err("path_extension expects path".into());
                }
                let p = match &a[0] {
                    Val::Str(s) => Path::new(s),
                    _ => return Err("path must be String".into()),
                };
                Val::Str(
                    p.extension()
                        .map(|x| x.to_string_lossy().to_string())
                        .unwrap_or_default(),
                )
            }
            "copy_file" => {
                if a.len() != 2 {
                    return Err("copy_file expects source,destination".into());
                }
                let src = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("source must be String".into()),
                };
                let dst = match &a[1] {
                    Val::Str(s) => s,
                    _ => return Err("destination must be String".into()),
                };
                Val::Num(fs::copy(src, dst).map_err(|e| e.to_string())? as f64)
            }
            "move_file" => {
                if a.len() != 2 {
                    return Err("move_file expects source,destination".into());
                }
                let src = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("source must be String".into()),
                };
                let dst = match &a[1] {
                    Val::Str(s) => s,
                    _ => return Err("destination must be String".into()),
                };
                fs::rename(src, dst).map_err(|e| e.to_string())?;
                Val::Null
            }
            "file_size" => {
                if a.len() != 1 {
                    return Err("file_size expects path".into());
                }
                let p = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("path must be String".into()),
                };
                Val::Num(fs::metadata(p).map_err(|e| e.to_string())?.len() as f64)
            }
            "is_file" => {
                if a.len() != 1 {
                    return Err("is_file expects path".into());
                }
                let p = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("path must be String".into()),
                };
                Val::Bool(Path::new(p).is_file())
            }
            "is_dir" => {
                if a.len() != 1 {
                    return Err("is_dir expects path".into());
                }
                let p = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("path must be String".into()),
                };
                Val::Bool(Path::new(p).is_dir())
            }
            "read_file" => {
                crate::runtime::require_cap("fs.read")?;
                if a.len() != 1 {
                    return Err("read_file expects path".into());
                }
                let p = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("path must be String".into()),
                };
                Val::Str(fs::read_to_string(p).map_err(|e| e.to_string())?)
            }
            "write_file" => {
                crate::runtime::require_cap("fs.write")?;
                if a.len() != 2 {
                    return Err("write_file expects path,data".into());
                }
                let p = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("path must be String".into()),
                };
                let data = a[1].show();
                fs::write(p, data).map_err(|e| e.to_string())?;
                Val::Null
            }
            "path_exists" => {
                if a.len() != 1 {
                    return Err("path_exists expects path".into());
                }
                let p = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("path must be String".into()),
                };
                Val::Bool(Path::new(p).exists())
            }
            "cwd" => Val::Str(
                env::current_dir()
                    .map_err(|e| e.to_string())?
                    .display()
                    .to_string(),
            ),
            "env_get" => {
                crate::runtime::require_cap("env")?;
                if a.len() != 1 {
                    return Err("env_get expects key".into());
                }
                let k = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("key must be String".into()),
                };
                env::var(k).map(Val::Str).unwrap_or(Val::Null)
            }
            "uuid_v4" => {
                if !a.is_empty() {
                    return Err("uuid_v4 expects no arguments".into());
                }
                Val::Str(uuid::Uuid::new_v4().to_string())
            }
            "base64_encode" => {
                if a.len() != 1 {
                    return Err("base64_encode expects value".into());
                }
                Val::Str(base64::engine::general_purpose::STANDARD.encode(a[0].show().as_bytes()))
            }
            "base64_decode" => {
                if a.len() != 1 {
                    return Err("base64_decode expects text".into());
                }
                let text = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("text must be String".into()),
                };
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(text)
                    .map_err(|e| e.to_string())?;
                Val::Str(String::from_utf8(bytes).map_err(|e| e.to_string())?)
            }
            "toml_parse" => {
                if a.len() != 1 {
                    return Err("toml_parse expects text".into());
                }
                let text = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("text must be String".into()),
                };
                let parsed: toml::Value = toml::from_str(text).map_err(|e| e.to_string())?;
                let json = serde_json::to_value(parsed).map_err(|e| e.to_string())?;
                json_to_val(&json)
            }
            "toml_stringify" => {
                if a.len() != 1 {
                    return Err("toml_stringify expects value".into());
                }
                Val::Str(toml::to_string(&val_to_json(&a[0])).map_err(|e| e.to_string())?)
            }
            "now_utc" => {
                if !a.is_empty() {
                    return Err("now_utc expects no arguments".into());
                }
                Val::Str(chrono::Utc::now().to_rfc3339())
            }
            "now_local" => {
                if !a.is_empty() {
                    return Err("now_local expects no arguments".into());
                }
                Val::Str(chrono::Local::now().to_rfc3339())
            }
            "datetime_format" => {
                if a.len() != 2 {
                    return Err("datetime_format expects timestamp,format".into());
                }
                let timestamp = match a[0] {
                    Val::Num(n) => n,
                    _ => return Err("timestamp must be Number".into()),
                };
                let format = match &a[1] {
                    Val::Str(s) => s,
                    _ => return Err("format must be String".into()),
                };
                let secs = timestamp.trunc() as i64;
                let nanos = ((timestamp.fract().abs()) * 1_000_000_000.0) as u32;
                let dt = chrono::DateTime::<chrono::Utc>::from_timestamp(secs, nanos)
                    .ok_or("invalid timestamp")?;
                Val::Str(dt.format(format).to_string())
            }
            "clock" => {
                let t = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(|e| e.to_string())?;
                Val::Num(t.as_secs_f64())
            }
            "random" => Val::Num(rand::thread_rng().r#gen::<f64>()),
            "randint" => {
                if a.len() != 2 {
                    return Err("randint expects min,max".into());
                }
                let lo = match a[0] {
                    Val::Num(n) => n as i64,
                    _ => return Err("min must be Number".into()),
                };
                let hi = match a[1] {
                    Val::Num(n) => n as i64,
                    _ => return Err("max must be Number".into()),
                };
                Val::Num(rand::thread_rng().gen_range(lo..=hi) as f64)
            }
            _ => return Ok(None),
        };
        Ok(Some(v))
    }
}
