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

fn wasm_bytes_from_val(v: &Val) -> Result<Vec<u8>, String> {
    match v {
        Val::List(xs) => xs
            .iter()
            .map(|b| match b {
                Val::Num(n) if *n >= 0.0 && *n <= 255.0 => Ok(*n as u8),
                _ => Err("wasm bytes List entries must be integers in 0..=255".to_string()),
            })
            .collect(),
        Val::Str(path) => fs::read(path).map_err(|e| format!("failed to read '{path}': {e}")),
        _ => Err("expected wasm byte List or .wasm file path String".into()),
    }
}

impl Vm {
    pub(crate) fn call_builtin_sys_net_conc_ffi(
        &mut self,
        n: &str,
        a: Vec<Val>,
    ) -> Result<Option<Val>, String> {
        let v = match n {
            "sha256" => {
                if a.len() != 1 {
                    return Err("sha256 expects value".into());
                }
                let mut h = Sha256::new();
                h.update(a[0].show().as_bytes());
                Val::Str(format!("{:x}", h.finalize()))
            }
            "hmac_sha256" => {
                if a.len() != 2 {
                    return Err("hmac_sha256 expects key,message".into());
                }
                let key = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("key must be String".into()),
                };
                let message = match &a[1] {
                    Val::Str(s) => s,
                    _ => return Err("message must be String".into()),
                };
                Val::Str(hmac_sha256_hex(key, message))
            }
            "pbkdf2_sha256" => {
                if a.len() != 4 {
                    return Err("pbkdf2_sha256 expects password,salt,iterations,bytes".into());
                }
                let password = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("password must be String".into()),
                };
                let salt = match &a[1] {
                    Val::Str(s) => s,
                    _ => return Err("salt must be String".into()),
                };
                let iterations = match a[2] {
                    Val::Num(n) => n.max(1.0) as u32,
                    _ => return Err("iterations must be Number".into()),
                };
                let bytes = match a[3] {
                    Val::Num(n) => n.max(1.0) as usize,
                    _ => return Err("bytes must be Number".into()),
                };
                Val::Str(pbkdf2_sha256_hex(password, salt, iterations, bytes))
            }
            "constant_time_equal" => {
                if a.len() != 2 {
                    return Err("constant_time_equal expects two strings".into());
                }
                let left = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("left must be String".into()),
                };
                let right = match &a[1] {
                    Val::Str(s) => s,
                    _ => return Err("right must be String".into()),
                };
                Val::Bool(constant_time_strings_equal(left, right))
            }
            "shell" => {
                crate::runtime::require_cap("process")?;
                if a.len() != 1 {
                    return Err("shell expects command".into());
                }
                let cmd = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("command must be String".into()),
                };
                let out = if cfg!(target_os = "windows") {
                    Command::new("cmd").args(["/C", cmd]).output()
                } else {
                    Command::new("sh").args(["-c", cmd]).output()
                }
                .map_err(|e| e.to_string())?;
                Val::Map(vec![
                    (
                        Val::Str("code".into()),
                        Val::Num(out.status.code().unwrap_or(-1) as f64),
                    ),
                    (
                        Val::Str("out".into()),
                        Val::Str(String::from_utf8_lossy(&out.stdout).to_string()),
                    ),
                    (
                        Val::Str("err".into()),
                        Val::Str(String::from_utf8_lossy(&out.stderr).to_string()),
                    ),
                ])
            }
            "powershell" | "ps_exec" => {
                crate::runtime::require_cap("process")?;
                if a.len() != 1 {
                    return Err(format!("{n} expects command"));
                }
                let Val::Str(cmd) = &a[0] else {
                    return Err("command must be String".into());
                };
                crate::term_ui::builtin_shell_val(crate::term_ui::ShellKind::PowerShell, cmd)?
            }
            "bash_exec" => {
                crate::runtime::require_cap("process")?;
                if a.len() != 1 {
                    return Err("bash_exec expects command".into());
                }
                let Val::Str(cmd) = &a[0] else {
                    return Err("command must be String".into());
                };
                crate::term_ui::builtin_shell_val(crate::term_ui::ShellKind::GitBash, cmd)?
            }
            #[cfg(feature = "native-services")]
            "http_request" => {
                crate::runtime::require_cap("net")?;
                if a.len() != 5 {
                    return Err("http_request expects method,url,headers,body,timeout_ms".into());
                }
                let method_text = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("method must be String".into()),
                };
                let url = match &a[1] {
                    Val::Str(s) => s,
                    _ => return Err("url must be String".into()),
                };
                let headers = match &a[2] {
                    Val::Map(p) => p,
                    _ => return Err("headers must be Map".into()),
                };
                let timeout = match a[4] {
                    Val::Num(n) => n.max(1.0) as u64,
                    _ => return Err("timeout_ms must be Number".into()),
                };
                let client = reqwest::blocking::Client::builder()
                    .timeout(std::time::Duration::from_millis(timeout))
                    .build()
                    .map_err(|e| e.to_string())?;
                let method = reqwest::Method::from_bytes(method_text.as_bytes())
                    .map_err(|e| e.to_string())?;
                let mut request = client.request(method, url);
                for (k, v) in headers {
                    request = request.header(k.show(), v.show());
                }
                if !matches!(a[3], Val::Null) {
                    let body = match &a[3] {
                        Val::Str(s) => s.clone(),
                        other => json_stringify_value(other)?,
                    };
                    request = request.body(body);
                }
                let response = request.send().map_err(|e| e.to_string())?;
                let status = response.status();
                let header_pairs = response
                    .headers()
                    .iter()
                    .map(|(k, v)| {
                        (
                            Val::Str(k.as_str().to_string()),
                            Val::Str(v.to_str().unwrap_or("").to_string()),
                        )
                    })
                    .collect::<Vec<_>>();
                let body = response.text().map_err(|e| e.to_string())?;
                Val::Map(vec![
                    (Val::Str("status".into()), Val::Num(status.as_u16() as f64)),
                    (Val::Str("ok".into()), Val::Bool(status.is_success())),
                    (Val::Str("headers".into()), Val::Map(header_pairs)),
                    (Val::Str("body".into()), Val::Str(body)),
                ])
            }
            #[cfg(feature = "native-services")]
            "http_get" => {
                crate::runtime::require_cap("net")?;
                if a.len() != 1 {
                    return Err("http_get expects url".into());
                }
                let url = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("url must be String".into()),
                };
                let text = reqwest::blocking::get(url)
                    .map_err(|e| e.to_string())?
                    .text()
                    .map_err(|e| e.to_string())?;
                Val::Str(text)
            }
            #[cfg(feature = "native-services")]
            "http_post" => {
                crate::runtime::require_cap("net")?;
                if a.len() != 2 {
                    return Err("http_post expects url,body".into());
                }
                let url = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("url must be String".into()),
                };
                let body = match &a[1] {
                    Val::Str(s) => s.clone(),
                    other => json_stringify_value(other)?,
                };
                let text = reqwest::blocking::Client::new()
                    .post(url)
                    .body(body)
                    .send()
                    .map_err(|e| e.to_string())?
                    .text()
                    .map_err(|e| e.to_string())?;
                Val::Str(text)
            }
            "sleep_ms" => {
                if a.len() != 1 {
                    return Err("sleep_ms expects milliseconds".into());
                }
                let ms = match a[0] {
                    Val::Num(n) => n.max(0.0) as u64,
                    _ => return Err("milliseconds must be Number".into()),
                };
                thread::sleep(std::time::Duration::from_millis(ms));
                Val::Null
            }
            "cpu_count" => Val::Num(
                thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(1) as f64,
            ),
            "platform_os" => Val::Str(env::consts::OS.to_string()),
            "platform_arch" => Val::Str(env::consts::ARCH.to_string()),
            "platform_family" => Val::Str(env::consts::FAMILY.to_string()),
            "path_separator" => Val::Str(std::path::MAIN_SEPARATOR.to_string()),
            #[cfg(feature = "native-services")]
            "sqlite_exec" => {
                if a.len() != 2 {
                    return Err("sqlite_exec expects path,sql".into());
                }
                let path = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("path must be String".into()),
                };
                let sql = match &a[1] {
                    Val::Str(s) => s,
                    _ => return Err("sql must be String".into()),
                };
                let conn = Connection::open(path).map_err(|e| e.to_string())?;
                let n = conn.execute(sql, []).map_err(|e| e.to_string())?;
                Val::Num(n as f64)
            }
            #[cfg(feature = "native-services")]
            "sqlite_query" => {
                if a.len() != 2 {
                    return Err("sqlite_query expects path,sql".into());
                }
                let path = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("path must be String".into()),
                };
                let sql = match &a[1] {
                    Val::Str(s) => s,
                    _ => return Err("sql must be String".into()),
                };
                let conn = Connection::open(path).map_err(|e| e.to_string())?;
                let mut stmt = conn.prepare(sql).map_err(|e| e.to_string())?;
                let names: Vec<String> =
                    stmt.column_names().iter().map(|s| s.to_string()).collect();
                let rows = stmt
                    .query_map([], |row| {
                        let mut pairs = Vec::new();
                        for (i, name) in names.iter().enumerate() {
                            let val = match row.get_ref(i)? {
                                rusqlite::types::ValueRef::Null => Val::Null,
                                rusqlite::types::ValueRef::Integer(x) => Val::Num(x as f64),
                                rusqlite::types::ValueRef::Real(x) => Val::Num(x),
                                rusqlite::types::ValueRef::Text(x) => {
                                    Val::Str(String::from_utf8_lossy(x).to_string())
                                }
                                rusqlite::types::ValueRef::Blob(x) => {
                                    Val::Str(format!("<{} bytes>", x.len()))
                                }
                            };
                            pairs.push((Val::Str(name.clone()), val));
                        }
                        Ok(Val::Map(pairs))
                    })
                    .map_err(|e| e.to_string())?;
                let mut out = Vec::new();
                for row in rows {
                    out.push(row.map_err(|e| e.to_string())?);
                }
                Val::List(out)
            }
            #[cfg(feature = "native-services")]
            "sqlite_exec_params" => {
                if a.len() != 3 {
                    return Err("sqlite_exec_params expects path,sql,params".into());
                }
                let path = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("path must be String".into()),
                };
                let sql = match &a[1] {
                    Val::Str(s) => s,
                    _ => return Err("sql must be String".into()),
                };
                let params = match &a[2] {
                    Val::List(v) => v,
                    _ => return Err("params must be List".into()),
                };
                let values: Vec<rusqlite::types::Value> =
                    params.iter().map(val_to_sql_value).collect();
                let conn = Connection::open(path).map_err(|e| e.to_string())?;
                let n = conn
                    .execute(sql, rusqlite::params_from_iter(values.iter()))
                    .map_err(|e| e.to_string())?;
                Val::Num(n as f64)
            }
            #[cfg(feature = "native-services")]
            "sqlite_query_params" => {
                if a.len() != 3 {
                    return Err("sqlite_query_params expects path,sql,params".into());
                }
                let path = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("path must be String".into()),
                };
                let sql = match &a[1] {
                    Val::Str(s) => s,
                    _ => return Err("sql must be String".into()),
                };
                let params = match &a[2] {
                    Val::List(v) => v,
                    _ => return Err("params must be List".into()),
                };
                let values: Vec<rusqlite::types::Value> =
                    params.iter().map(val_to_sql_value).collect();
                let conn = Connection::open(path).map_err(|e| e.to_string())?;
                let mut stmt = conn.prepare(sql).map_err(|e| e.to_string())?;
                let names: Vec<String> =
                    stmt.column_names().iter().map(|s| s.to_string()).collect();
                let rows = stmt
                    .query_map(rusqlite::params_from_iter(values.iter()), |row| {
                        let mut pairs = Vec::new();
                        for (i, name) in names.iter().enumerate() {
                            let val = match row.get_ref(i)? {
                                rusqlite::types::ValueRef::Null => Val::Null,
                                rusqlite::types::ValueRef::Integer(x) => Val::Num(x as f64),
                                rusqlite::types::ValueRef::Real(x) => Val::Num(x),
                                rusqlite::types::ValueRef::Text(x) => {
                                    Val::Str(String::from_utf8_lossy(x).to_string())
                                }
                                rusqlite::types::ValueRef::Blob(x) => {
                                    Val::Str(format!("<{} bytes>", x.len()))
                                }
                            };
                            pairs.push((Val::Str(name.clone()), val));
                        }
                        Ok(Val::Map(pairs))
                    })
                    .map_err(|e| e.to_string())?;
                let mut out = Vec::new();
                for row in rows {
                    out.push(row.map_err(|e| e.to_string())?);
                }
                Val::List(out)
            }
            "spawn" => {
                if a.len() != 2 {
                    return Err("spawn expects callable,args".into());
                }
                let (_, def) = self.callable_def(&a[0])?;
                let args = match &a[1] {
                    Val::List(v) => v.clone(),
                    _ => return Err("spawn args must be List".into()),
                };
                self.spawn_task_with_kind(def, args, "task")?
            }
            "task_done" => {
                if a.len() != 1 {
                    return Err("task_done expects task id".into());
                }
                let id = match a[0] {
                    Val::Num(n) => n as u64,
                    _ => return Err("task id must be Number".into()),
                };
                let guard = tasks().lock().map_err(|_| "task registry poisoned")?;
                Val::Bool(guard.get(&id).map(|h| h.is_finished()).unwrap_or(true))
            }
            "task_status" => {
                if a.len() != 1 {
                    return Err("task_status expects task id".into());
                }
                let id = match a[0] {
                    Val::Num(n) => n as u64,
                    _ => return Err("task id must be Number".into()),
                };
                let guard = tasks().lock().map_err(|_| "task registry poisoned")?;
                Val::Str(
                    match guard.get(&id) {
                        Some(h) if h.is_finished() => "finished",
                        Some(_) => "running",
                        None => "consumed",
                    }
                    .into(),
                )
            }
            "await_timeout" => {
                if a.len() != 2 {
                    return Err("await_timeout expects task_id,milliseconds".into());
                }
                let id = match a[0] {
                    Val::Num(n) => n as u64,
                    _ => return Err("task id must be Number".into()),
                };
                let ms = match a[1] {
                    Val::Num(n) if n >= 0.0 => n as u64,
                    _ => return Err("timeout must be a non-negative Number".into()),
                };
                let deadline = Instant::now() + Duration::from_millis(ms);
                loop {
                    let finished = {
                        let guard = tasks().lock().map_err(|_| "task registry poisoned")?;
                        match guard.get(&id) {
                            Some(h) => h.is_finished(),
                            None => return Err(format!("unknown task {id}")),
                        }
                    };
                    if finished {
                        let handle = tasks()
                            .lock()
                            .map_err(|_| "task registry poisoned")?
                            .remove(&id)
                            .ok_or(format!("unknown task {id}"))?;
                        break handle.join().map_err(|_| "task panicked".to_string())??;
                    }
                    if Instant::now() >= deadline {
                        break Val::Null;
                    }
                    thread::sleep(Duration::from_millis(1));
                }
            }
            "await_task" => {
                if a.len() != 1 {
                    return Err("await_task expects task id".into());
                }
                let id = match a[0] {
                    Val::Num(n) => n as u64,
                    _ => return Err("task id must be Number".into()),
                };
                let handle = tasks()
                    .lock()
                    .map_err(|_| "task registry poisoned")?
                    .remove(&id)
                    .ok_or(format!("unknown task {id}"))?;
                handle.join().map_err(|_| "task panicked".to_string())??
            }
            "gather" => {
                if a.len() != 1 {
                    return Err("gather expects list of task ids".into());
                }
                let ids = match &a[0] {
                    Val::List(v) => v.clone(),
                    _ => return Err("gather expects List".into()),
                };
                let mut out = vec![];
                for task in ids {
                    let id = match task {
                        Val::Num(n) => n as u64,
                        _ => return Err("gather task ids must be Numbers".into()),
                    };
                    let handle = tasks()
                        .lock()
                        .map_err(|_| "task registry poisoned")?
                        .remove(&id)
                        .ok_or(format!("unknown task {id}"))?;
                    out.push(handle.join().map_err(|_| "task panicked".to_string())??);
                }
                Val::List(out)
            }
            "channel" => {
                if !a.is_empty() {
                    return Err("channel expects no arguments".into());
                }
                let (tx, rx) = mpsc::channel::<Val>();
                let id = CHANNEL_IDS.fetch_add(1, Ordering::Relaxed);
                channels()
                    .lock()
                    .map_err(|_| "channel registry poisoned")?
                    .insert(id, (tx, Arc::new(Mutex::new(rx))));
                Val::Num(id as f64)
            }
            "send" => {
                if a.len() != 2 {
                    return Err("send expects channel,value".into());
                }
                let id = match a[0] {
                    Val::Num(n) => n as u64,
                    _ => return Err("channel id must be Number".into()),
                };
                let tx = channels()
                    .lock()
                    .map_err(|_| "channel registry poisoned")?
                    .get(&id)
                    .map(|(tx, _)| tx.clone())
                    .ok_or(format!("unknown channel {id}"))?;
                tx.send(a[1].clone()).map_err(|e| e.to_string())?;
                Val::Null
            }
            "recv" => {
                if a.len() != 1 {
                    return Err("recv expects channel".into());
                }
                let id = match a[0] {
                    Val::Num(n) => n as u64,
                    _ => return Err("channel id must be Number".into()),
                };
                let rx = channels()
                    .lock()
                    .map_err(|_| "channel registry poisoned")?
                    .get(&id)
                    .map(|(_, rx)| rx.clone())
                    .ok_or(format!("unknown channel {id}"))?;
                let value = rx
                    .lock()
                    .map_err(|_| "channel receiver poisoned")?
                    .recv()
                    .map_err(|e| e.to_string())?;
                value
            }
            "try_recv" => {
                if a.len() != 1 {
                    return Err("try_recv expects channel".into());
                }
                let id = match a[0] {
                    Val::Num(n) => n as u64,
                    _ => return Err("channel id must be Number".into()),
                };
                let rx = channels()
                    .lock()
                    .map_err(|_| "channel registry poisoned")?
                    .get(&id)
                    .map(|(_, rx)| rx.clone())
                    .ok_or(format!("unknown channel {id}"))?;
                match rx
                    .lock()
                    .map_err(|_| "channel receiver poisoned")?
                    .try_recv()
                {
                    Ok(v) => v,
                    Err(TryRecvError::Empty) => Val::Null,
                    Err(TryRecvError::Disconnected) => {
                        return Err("channel disconnected".into());
                    }
                }
            }
            "ffi_available" => {
                if a.len() != 1 {
                    return Err("ffi_available expects library path".into());
                }
                let path = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("library path must be String".into()),
                };
                Val::Bool(unsafe { Library::new(path) }.is_ok())
            }
            "ffi_i32" => {
                if a.len() != 3 {
                    return Err("ffi_i32 expects library,symbol,args".into());
                }
                let path = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("library path must be String".into()),
                };
                let symbol = match &a[1] {
                    Val::Str(s) => s,
                    _ => return Err("symbol must be String".into()),
                };
                let vals = match &a[2] {
                    Val::List(v) => v,
                    _ => return Err("ffi args must be List".into()),
                };
                let nums: Result<Vec<i32>, String> = vals
                    .iter()
                    .map(|v| match v {
                        Val::Num(n) => Ok(*n as i32),
                        _ => Err("ffi_i32 arguments must be Numbers".into()),
                    })
                    .collect();
                let nums = nums?;
                let mut sym = symbol.as_bytes().to_vec();
                sym.push(0);
                let lib = unsafe { Library::new(path) }.map_err(|e| e.to_string())?;
                let result: i32 = unsafe {
                    match nums.as_slice() {
                        [] => {
                            let f: libloading::Symbol<unsafe extern "C" fn() -> i32> =
                                lib.get(&sym).map_err(|e| e.to_string())?;
                            f()
                        }
                        [a] => {
                            let f: libloading::Symbol<unsafe extern "C" fn(i32) -> i32> =
                                lib.get(&sym).map_err(|e| e.to_string())?;
                            f(*a)
                        }
                        [a, b] => {
                            let f: libloading::Symbol<unsafe extern "C" fn(i32, i32) -> i32> =
                                lib.get(&sym).map_err(|e| e.to_string())?;
                            f(*a, *b)
                        }
                        [a, b, c] => {
                            let f: libloading::Symbol<unsafe extern "C" fn(i32, i32, i32) -> i32> =
                                lib.get(&sym).map_err(|e| e.to_string())?;
                            f(*a, *b, *c)
                        }
                        [a, b, c, e] => {
                            let f: libloading::Symbol<
                                unsafe extern "C" fn(i32, i32, i32, i32) -> i32,
                            > = lib.get(&sym).map_err(|e| e.to_string())?;
                            f(*a, *b, *c, *e)
                        }
                        _ => {
                            return Err("ffi_i32 supports 0..4 integer arguments".into());
                        }
                    }
                };
                Val::Num(result as f64)
            }
            "ffi_i64" => {
                if a.len() != 3 {
                    return Err("ffi_i64 expects library,symbol,args".into());
                }
                let path = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("library path must be String".into()),
                };
                let symbol = match &a[1] {
                    Val::Str(s) => s,
                    _ => return Err("symbol must be String".into()),
                };
                let vals = match &a[2] {
                    Val::List(v) => v,
                    _ => return Err("ffi args must be List".into()),
                };
                let nums: Result<Vec<i64>, String> = vals
                    .iter()
                    .map(|v| match v {
                        Val::Num(n) => Ok(*n as i64),
                        _ => Err("ffi_i64 arguments must be Numbers".into()),
                    })
                    .collect();
                let nums = nums?;
                let mut sym = symbol.as_bytes().to_vec();
                sym.push(0);
                let lib = unsafe { Library::new(path) }.map_err(|e| e.to_string())?;
                let result: i64 = unsafe {
                    match nums.as_slice() {
                        [] => {
                            let f: libloading::Symbol<unsafe extern "C" fn() -> i64> =
                                lib.get(&sym).map_err(|e| e.to_string())?;
                            f()
                        }
                        [a] => {
                            let f: libloading::Symbol<unsafe extern "C" fn(i64) -> i64> =
                                lib.get(&sym).map_err(|e| e.to_string())?;
                            f(*a)
                        }
                        [a, b] => {
                            let f: libloading::Symbol<unsafe extern "C" fn(i64, i64) -> i64> =
                                lib.get(&sym).map_err(|e| e.to_string())?;
                            f(*a, *b)
                        }
                        _ => {
                            return Err("ffi_i64 supports 0..2 integer arguments".into());
                        }
                    }
                };
                Val::Num(result as f64)
            }
            "ffi_f64" => {
                if a.len() != 3 {
                    return Err("ffi_f64 expects library,symbol,args".into());
                }
                let path = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("library path must be String".into()),
                };
                let symbol = match &a[1] {
                    Val::Str(s) => s,
                    _ => return Err("symbol must be String".into()),
                };
                let vals = match &a[2] {
                    Val::List(v) => v,
                    _ => return Err("ffi args must be List".into()),
                };
                let nums: Result<Vec<f64>, String> = vals
                    .iter()
                    .map(|v| match v {
                        Val::Num(n) => Ok(*n),
                        _ => Err("ffi_f64 arguments must be Numbers".into()),
                    })
                    .collect();
                let nums = nums?;
                let mut sym = symbol.as_bytes().to_vec();
                sym.push(0);
                let lib = unsafe { Library::new(path) }.map_err(|e| e.to_string())?;
                let result: f64 = unsafe {
                    match nums.as_slice() {
                        [] => {
                            let f: libloading::Symbol<unsafe extern "C" fn() -> f64> =
                                lib.get(&sym).map_err(|e| e.to_string())?;
                            f()
                        }
                        [a] => {
                            let f: libloading::Symbol<unsafe extern "C" fn(f64) -> f64> =
                                lib.get(&sym).map_err(|e| e.to_string())?;
                            f(*a)
                        }
                        [a, b] => {
                            let f: libloading::Symbol<unsafe extern "C" fn(f64, f64) -> f64> =
                                lib.get(&sym).map_err(|e| e.to_string())?;
                            f(*a, *b)
                        }
                        _ => {
                            return Err("ffi_f64 supports 0..2 floating arguments".into());
                        }
                    }
                };
                Val::Num(result)
            }
            "py_available" => {
                if !a.is_empty() {
                    return Err("py_available expects 0 arguments".into());
                }
                Val::Bool(crate::py_bridge::py_available())
            }
            "py_import" => {
                crate::runtime::require_cap("python")?;
                if a.len() != 1 {
                    return Err("py_import expects module name String".into());
                }
                let Val::Str(name) = &a[0] else {
                    return Err("py_import module name must be String".into());
                };
                crate::py_bridge::py_import(name)?
            }
            "py_call" => {
                if a.is_empty() || a.len() > 3 {
                    return Err("py_call expects (callable[, args[, kwargs]])".into());
                }
                let args = match a.get(1) {
                    None | Some(Val::Null) => Vec::new(),
                    Some(Val::List(xs)) => xs.clone(),
                    _ => return Err("py_call args must be List".into()),
                };
                crate::py_bridge::py_call(&a[0], &args, a.get(2))?
            }
            "py_get" => {
                if a.len() != 2 {
                    return Err("py_get expects (py_obj, attr)".into());
                }
                let Val::Str(attr) = &a[1] else {
                    return Err("py_get attr must be String".into());
                };
                crate::py_bridge::py_get(&a[0], attr)?
            }
            "py_set" => {
                if a.len() != 3 {
                    return Err("py_set expects (py_obj, attr, value)".into());
                }
                let Val::Str(attr) = &a[1] else {
                    return Err("py_set attr must be String".into());
                };
                crate::py_bridge::py_set(&a[0], attr, &a[2])?
            }
            "py_dir" => {
                if a.len() != 1 {
                    return Err("py_dir expects py_obj".into());
                }
                crate::py_bridge::py_dir(&a[0])?
            }
            "py_eval" => {
                crate::runtime::require_cap("python")?;
                if a.is_empty() || a.len() > 2 {
                    return Err("py_eval expects (expr[, locals])".into());
                }
                let Val::Str(expr) = &a[0] else {
                    return Err("py_eval expr must be String".into());
                };
                crate::py_bridge::py_eval(expr, a.get(1))?
            }
            "py_exec" => {
                crate::runtime::require_cap("python")?;
                if a.len() != 1 {
                    return Err("py_exec expects Python source String".into());
                }
                let Val::Str(code) = &a[0] else {
                    return Err("py_exec source must be String".into());
                };
                crate::py_bridge::py_exec(code)?
            }
            "py_info" => {
                if !a.is_empty() {
                    return Err("py_info expects no args".into());
                }
                crate::py_bridge::py_info_val()
            }
            "py_free" => {
                if a.len() != 1 {
                    return Err("py_free expects Python handle".into());
                }
                Val::Bool(crate::py_bridge::py_free(&a[0])?)
            }
            "py_to_tensor" => {
                if a.len() != 1 {
                    return Err("py_to_tensor expects Python buffer/ndarray handle".into());
                }
                crate::py_bridge::py_to_tensor(&a[0])?
            }
            "tensor_to_py" => {
                crate::runtime::require_cap("python")?;
                if a.len() != 1 {
                    return Err("tensor_to_py expects Tensor".into());
                }
                crate::py_bridge::tensor_to_py(&a[0])?
            }
            "tensor_to_dlpack" => {
                crate::runtime::require_cap("python")?;
                if a.len() != 1 {
                    return Err("tensor_to_dlpack expects Tensor".into());
                }
                crate::py_bridge::tensor_to_dlpack(&a[0])?
            }
            "py_from_dlpack" => {
                crate::runtime::require_cap("python")?;
                if a.len() != 1 {
                    return Err("py_from_dlpack expects DLPack Python object/PyCapsule".into());
                }
                crate::py_bridge::py_from_dlpack(&a[0])?
            }
            "jit_available" => {
                if !a.is_empty() {
                    return Err("jit_available expects 0 arguments".into());
                }
                Val::Bool(crate::jit::jit_available())
            }
            "jit_info" | "jit_stats" => {
                if !a.is_empty() {
                    return Err(format!("{n} expects 0 arguments"));
                }
                crate::jit::jit_info_val()
            }
            "jit_set_enabled" => {
                if a.len() != 1 {
                    return Err("jit_set_enabled expects Bool".into());
                }
                Val::Bool(crate::jit::jit_set_enabled(a[0].truthy()))
            }
            "jit_compile" => {
                if a.len() != 1 {
                    return Err("jit_compile expects function or function name".into());
                }
                let (name, def) = match &a[0] {
                    Val::Function(key) => {
                        let def = self
                            .funcs
                            .get(key)
                            .cloned()
                            .or_else(|| resolve_function_object(key).ok().flatten())
                            .ok_or_else(|| format!("unknown function reference '{key}'"))?;
                        (def.debug_name.clone(), def)
                    }
                    Val::Str(fn_name) => {
                        let def = match self.vars.get(fn_name) {
                            Some(Val::Function(key)) => self
                                .funcs
                                .get(key)
                                .cloned()
                                .or_else(|| resolve_function_object(key).ok().flatten()),
                            _ => self.funcs.get(fn_name).cloned(),
                        }
                        .ok_or_else(|| format!("unknown function '{fn_name}'"))?;
                        (fn_name.clone(), def)
                    }
                    _ => return Err("jit_compile expects Function or String".into()),
                };
                let ok = crate::jit::jit_try_compile(&name, &def, &|fname| {
                    self.resolve_jit_callee(fname)
                })
                .is_ok();
                Val::Bool(ok)
            }
            "plugin_load" => {
                crate::runtime::require_cap("ffi")?;
                if a.len() != 1 {
                    return Err("plugin_load expects library path String".into());
                }
                let Val::Str(path) = &a[0] else {
                    return Err("plugin_load path must be String".into());
                };
                crate::plugin::load_plugin(Path::new(path))?.info_val()
            }
            "plugin_info" => {
                if a.len() != 1 {
                    return Err("plugin_info expects plugin handle or name".into());
                }
                let key = match &a[0] {
                    Val::Str(s) => s.clone(),
                    Val::Map(pairs) => map_lookup(pairs, &Val::Str("id".into()))
                        .and_then(|v| {
                            if let Val::Str(s) = v {
                                Some(s.clone())
                            } else {
                                None
                            }
                        })
                        .ok_or("invalid plugin handle Map")?,
                    _ => return Err("plugin_info expects String or plugin Map".into()),
                };
                crate::plugin::get_loaded_plugin(&key)?.info_val()
            }
            "plugin_list" => {
                if !a.is_empty() {
                    return Err("plugin_list expects 0 arguments".into());
                }
                Val::List(crate::plugin::list_loaded_plugins())
            }
            "plugin_call" => {
                if a.len() < 2 || a.len() > 3 {
                    return Err("plugin_call expects (plugin, fn_name[, args])".into());
                }
                let key = match &a[0] {
                    Val::Str(s) => s.clone(),
                    Val::Map(pairs) => map_lookup(pairs, &Val::Str("id".into()))
                        .and_then(|v| {
                            if let Val::Str(s) = v {
                                Some(s.clone())
                            } else {
                                None
                            }
                        })
                        .ok_or("invalid plugin handle Map")?,
                    _ => return Err("plugin_call expects String or plugin Map".into()),
                };
                let Val::Str(fn_name) = &a[1] else {
                    return Err("plugin_call function name must be String".into());
                };
                let call_args = match a.get(2) {
                    None => Vec::new(),
                    Some(Val::List(xs)) => xs.clone(),
                    Some(_) => return Err("plugin_call args must be List".into()),
                };
                let p = crate::plugin::get_loaded_plugin(&key)?;
                p.call(fn_name, &call_args)?
            }
            "plugin_build" => {
                if a.is_empty() || a.len() > 2 {
                    return Err("plugin_build expects (source_rs_path[, out_lib_path])".into());
                }
                let Val::Str(src) = &a[0] else {
                    return Err("plugin_build source path must be String".into());
                };
                let src_path = PathBuf::from(src);
                let out_path = match a.get(1) {
                    Some(Val::Str(o)) => PathBuf::from(o),
                    Some(_) => return Err("plugin_build out path must be String".into()),
                    None => {
                        let stem = src_path
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or("kovrek_plugin");
                        src_path.with_file_name(format!(
                            "{stem}.{}",
                            crate::plugin::platform_shared_ext()
                        ))
                    }
                };
                let built = crate::plugin::build_rust_plugin(&src_path, &out_path)?;
                Val::Str(built.to_string_lossy().to_string())
            }
            "__plugin_trampoline" => {
                if a.len() < 2 {
                    return Err("__plugin_trampoline expects plugin_id,fn_name,...".into());
                }
                let (Val::Str(pid), Val::Str(fname)) = (&a[0], &a[1]) else {
                    return Err("invalid plugin trampoline arguments".into());
                };
                crate::plugin::get_loaded_plugin(pid)?.call(fname, &a[2..])?
            }
            "cap_current" => {
                if !a.is_empty() {
                    return Err("cap_current expects 0 arguments".into());
                }
                match crate::runtime::current_caps() {
                    None => Val::Str("all".into()),
                    Some(list) => Val::List(list.into_iter().map(Val::Str).collect()),
                }
            }
            "cap_restrict" => {
                if a.len() != 2 {
                    return Err("cap_restrict expects (caps_list_or_csv, fn)".into());
                }
                let caps: Vec<String> = match &a[0] {
                    Val::Str(s) => s
                        .split(',')
                        .map(|x| x.trim().to_string())
                        .filter(|x| !x.is_empty())
                        .collect(),
                    Val::List(xs) => xs
                        .iter()
                        .map(|v| match v {
                            Val::Str(s) => Ok(s.trim().to_string()),
                            _ => Err("cap_restrict capability names must be String".to_string()),
                        })
                        .collect::<Result<Vec<_>, _>>()?,
                    _ => return Err("cap_restrict caps must be String or List".into()),
                };
                let callee = a[1].clone();
                crate::runtime::run_with_caps(caps, || self.invoke_callable_sync(&callee, vec![]))?
            }
            "wasm_compile" => {
                if a.is_empty() || a.len() > 2 {
                    return Err("wasm_compile expects (source_string[, wasi])".into());
                }
                let Val::Str(src) = &a[0] else {
                    return Err("wasm_compile source must be String".into());
                };
                let wasi = match a.get(1) {
                    None => false,
                    Some(Val::Bool(b)) => *b,
                    Some(_) => return Err("wasm_compile wasi flag must be Bool".into()),
                };
                let bytes = crate::native_codegen::compile_source_to_wasm_with_opts(src, wasi)?;
                Val::List(bytes.into_iter().map(|b| Val::Num(f64::from(b))).collect())
            }
            "wasm_disasm" => {
                if a.len() != 1 {
                    return Err("wasm_disasm expects (wasm_bytes_or_path)".into());
                }
                let bytes = wasm_bytes_from_val(&a[0])?;
                Val::Str(crate::native_codegen::wasm_disasm(&bytes)?)
            }
            "wasm_run" => {
                if a.is_empty() || a.len() > 3 {
                    return Err("wasm_run expects (wasm_bytes_or_path[, fn_name[, args]])".into());
                }
                let bytes = wasm_bytes_from_val(&a[0])?;
                let fn_name = match a.get(1) {
                    None | Some(Val::Null) => "main".to_string(),
                    Some(Val::Str(s)) => s.clone(),
                    Some(_) => return Err("wasm_run fn_name must be String".into()),
                };
                let args = match a.get(2) {
                    None | Some(Val::Null) => Vec::new(),
                    Some(Val::List(xs)) => xs
                        .iter()
                        .map(|v| match v {
                            Val::Num(n) => Ok(*n),
                            Val::Bool(b) => Ok(if *b { 1.0 } else { 0.0 }),
                            _ => Err("wasm_run args must be numeric".to_string()),
                        })
                        .collect::<Result<Vec<_>, _>>()?,
                    Some(_) => return Err("wasm_run args must be List".into()),
                };
                let (res, prints) = crate::native_codegen::wasm_run(&bytes, &fn_name, &args)?;
                Val::Map(vec![
                    (Val::Str("result".into()), Val::Num(res)),
                    (
                        Val::Str("prints".into()),
                        Val::List(prints.into_iter().map(Val::Str).collect()),
                    ),
                ])
            }
            "__plugin_trampoline_list" => {
                if a.len() != 3 {
                    return Err("__plugin_trampoline_list expects plugin_id,fn_name,args".into());
                }
                let (Val::Str(pid), Val::Str(fname), Val::List(xs)) = (&a[0], &a[1], &a[2]) else {
                    return Err("invalid plugin list trampoline arguments".into());
                };
                crate::plugin::get_loaded_plugin(pid)?.call(fname, xs)?
            }
            "gc_collect" => {
                if a.len() > 1 {
                    return Err("gc_collect expects ([mode])".into());
                }
                match a.first() {
                    None | Some(Val::Null) => self.gc_collect()?,
                    Some(Val::Str(mode)) => self.gc_collect_mode(Some(mode))?,
                    Some(_) => {
                        return Err("gc_collect mode must be String ('minor' or 'major')".into());
                    }
                }
            }
            "arena_scope" => {
                if a.len() != 1 {
                    return Err("arena_scope expects (callable)".into());
                }
                self.arena_scope(a[0].clone())?
            }
            "gc_stats" => {
                if !a.is_empty() {
                    return Err("gc_stats expects 0 arguments".into());
                }
                self.gc_stats()?
            }
            "gc_pause" => {
                if !a.is_empty() {
                    return Err("gc_pause expects 0 arguments".into());
                }
                self.gc_set_paused(true)
            }
            "gc_resume" => {
                if !a.is_empty() {
                    return Err("gc_resume expects 0 arguments".into());
                }
                self.gc_set_paused(false)
            }
            "profile_start" => {
                if !a.is_empty() {
                    return Err("profile_start expects 0 arguments".into());
                }
                self.profile_start()
            }
            "profile_stop" => {
                if !a.is_empty() {
                    return Err("profile_stop expects 0 arguments".into());
                }
                self.profile_stop()?
            }
            "profile_report" => {
                if !a.is_empty() {
                    return Err("profile_report expects 0 arguments".into());
                }
                self.profile_report()?
            }
            "sim_start" => {
                if a.len() != 1 {
                    return Err("sim_start expects integer seed".into());
                }
                let seed = match a[0] {
                    Val::Num(n) if n.is_finite() => n as i64 as u64,
                    _ => return Err("sim_start seed must be Number".into()),
                };
                crate::sim_actor::sim_start(seed)
            }
            "sim_stop" => {
                if !a.is_empty() {
                    return Err("sim_stop expects 0 arguments".into());
                }
                crate::sim_actor::sim_stop()
            }
            "sim_now" => {
                if !a.is_empty() {
                    return Err("sim_now expects 0 arguments".into());
                }
                crate::sim_actor::sim_now()?
            }
            "sim_advance" => {
                if a.len() != 1 {
                    return Err("sim_advance expects milliseconds".into());
                }
                let Val::Num(ms) = a[0] else {
                    return Err("sim_advance expects Number milliseconds".into());
                };
                crate::sim_actor::sim_advance(ms)?
            }
            "sim_rand" => {
                if !a.is_empty() {
                    return Err("sim_rand expects 0 arguments".into());
                }
                crate::sim_actor::sim_rand()?
            }
            "sim_fault" => {
                if a.is_empty() || a.len() > 2 {
                    return Err("sim_fault expects (target[, rate])".into());
                }
                let Val::Str(ref target) = a[0] else {
                    return Err("sim_fault target must be String".into());
                };
                if a.len() == 2 {
                    let Val::Num(rate) = a[1] else {
                        return Err("sim_fault rate must be Number".into());
                    };
                    crate::sim_actor::sim_fault_set(target, rate)?
                } else {
                    crate::sim_actor::sim_fault_check(target)?
                }
            }
            "sim_log" => {
                if a.len() > 1 {
                    return Err("sim_log expects 0 or 1 argument".into());
                }
                if a.is_empty() {
                    crate::sim_actor::sim_log_get()?
                } else {
                    crate::sim_actor::sim_log_append(a[0].clone())?
                }
            }
            "sim_actor_spawn" => match a.len() {
                1 => crate::sim_actor::sim_actor_spawn(self, None, a[0].clone(), None)?,
                2 => {
                    if let Val::Str(ref name) = a[0] {
                        crate::sim_actor::sim_actor_spawn(self, Some(name), a[1].clone(), None)?
                    } else {
                        crate::sim_actor::sim_actor_spawn(self, None, a[0].clone(), Some(&a[1]))?
                    }
                }
                3 => {
                    let Val::Str(ref name) = a[0] else {
                        return Err("sim_actor_spawn first argument must be String name".into());
                    };
                    crate::sim_actor::sim_actor_spawn(self, Some(name), a[1].clone(), Some(&a[2]))?
                }
                _ => {
                    return Err(
                        "sim_actor_spawn expects ([name,] handler[, options_or_init])".into(),
                    );
                }
            },
            "sim_actor_send" => match a.len() {
                2 => {
                    let Val::Str(ref to) = a[0] else {
                        return Err("sim_actor_send target must be String name".into());
                    };
                    crate::sim_actor::sim_actor_send("__host__", to, a[1].clone(), 1)?
                }
                3 => {
                    let (Val::Str(from), Val::Str(to)) = (&a[0], &a[1]) else {
                        return Err(
                            "sim_actor_send (from, to, payload) expects String actor names".into(),
                        );
                    };
                    crate::sim_actor::sim_actor_send(from, to, a[2].clone(), 1)?
                }
                4 => {
                    let (Val::Str(from), Val::Str(to), Val::Num(delay)) = (&a[0], &a[1], &a[3])
                    else {
                        return Err(
                            "sim_actor_send expects (from, to, payload[, delay_ticks])".into()
                        );
                    };
                    if !delay.is_finite() || *delay < 0.0 {
                        return Err("sim_actor_send delay_ticks must be non-negative Number".into());
                    }
                    crate::sim_actor::sim_actor_send(from, to, a[2].clone(), *delay as u64)?
                }
                _ => {
                    return Err(
                        "sim_actor_send expects ([from,] to, payload[, delay_ticks])".into(),
                    );
                }
            },
            "sim_step" => {
                if !a.is_empty() {
                    return Err("sim_step expects 0 arguments".into());
                }
                crate::sim_actor::sim_step()?
            }
            "sim_run_until" => {
                if a.len() > 1 {
                    return Err("sim_run_until expects ([until_tick])".into());
                }
                let until = match a.first() {
                    None | Some(Val::Null) => None,
                    Some(Val::Num(n)) if n.is_finite() && *n >= 0.0 => Some(*n as u64),
                    Some(_) => {
                        return Err("sim_run_until until_tick must be non-negative Number".into());
                    }
                };
                crate::sim_actor::sim_run_until(until)?
            }
            "sim_partition" => {
                if a.len() < 2 || a.len() > 3 {
                    return Err("sim_partition expects (actor_a, actor_b[, drop_prob])".into());
                }
                let (Val::Str(a_name), Val::Str(b_name)) = (&a[0], &a[1]) else {
                    return Err("sim_partition actor names must be String".into());
                };
                let prob = match a.get(2) {
                    None | Some(Val::Null) => 1.0,
                    Some(Val::Num(p)) => *p,
                    Some(_) => return Err("sim_partition drop_prob must be Number".into()),
                };
                crate::sim_actor::sim_partition(a_name, b_name, prob)?
            }
            "sim_actor_state" => {
                if a.len() != 1 {
                    return Err("sim_actor_state expects (actor_name)".into());
                }
                let Val::Str(ref name) = a[0] else {
                    return Err("sim_actor_state name must be String".into());
                };
                crate::sim_actor::sim_actor_state(name)?
            }
            "sim_trace" => {
                if !a.is_empty() {
                    return Err("sim_trace expects 0 arguments".into());
                }
                crate::sim_actor::sim_trace()?
            }
            "sim_fs_write" => {
                if a.len() != 2 {
                    return Err("sim_fs_write expects (path, content)".into());
                }
                let (Val::Str(path), Val::Str(content)) = (&a[0], &a[1]) else {
                    return Err("sim_fs_write expects String arguments".into());
                };
                crate::sim_actor::sim_fs_write(path, content)?
            }
            "sim_fs_read" => {
                if a.len() != 1 {
                    return Err("sim_fs_read expects (path)".into());
                }
                let Val::Str(ref path) = a[0] else {
                    return Err("sim_fs_read path must be String".into());
                };
                crate::sim_actor::sim_fs_read(path)?
            }
            "sim_fs_delete" => {
                if a.len() != 1 {
                    return Err("sim_fs_delete expects (path)".into());
                }
                let Val::Str(ref path) = a[0] else {
                    return Err("sim_fs_delete path must be String".into());
                };
                crate::sim_actor::sim_fs_delete(path)?
            }
            "sim_fs_list" => {
                if a.len() > 1 {
                    return Err("sim_fs_list expects ([prefix])".into());
                }
                let prefix = match a.first() {
                    None | Some(Val::Null) => None,
                    Some(Val::Str(s)) => Some(s.as_str()),
                    Some(_) => return Err("sim_fs_list prefix must be String".into()),
                };
                crate::sim_actor::sim_fs_list(prefix)?
            }
            "sim_fs_sync" => {
                if !a.is_empty() {
                    return Err("sim_fs_sync expects 0 arguments".into());
                }
                crate::sim_actor::sim_fs_sync()?
            }
            "sim_fs_crash" => {
                if !a.is_empty() {
                    return Err("sim_fs_crash expects 0 arguments".into());
                }
                crate::sim_actor::sim_fs_crash()?
            }
            "sim_fs_snapshot" => {
                if !a.is_empty() {
                    return Err("sim_fs_snapshot expects 0 arguments".into());
                }
                crate::sim_actor::sim_fs_snapshot()?
            }
            "actor_node_listen" => {
                crate::runtime::require_cap("net")?;
                if a.is_empty() || a.len() > 2 {
                    return Err("actor_node_listen expects (actor_id[, port_or_addr])".into());
                }
                let Val::Num(id) = a[0] else {
                    return Err("actor_node_listen actor_id must be Number".into());
                };
                if !id.is_finite() || id < 0.0 {
                    return Err("actor_node_listen actor_id must be non-negative Number".into());
                }
                crate::sim_actor::actor_node_listen(id as u64, a.get(1))?
            }
            "actor_node_stop" => {
                if a.len() != 1 {
                    return Err("actor_node_stop expects (node_id_or_map)".into());
                }
                crate::sim_actor::actor_node_stop(&a[0])?
            }
            "actor_remote_send" => {
                if a.len() != 2 {
                    return Err("actor_remote_send expects (target, message)".into());
                }
                crate::sim_actor::actor_remote_send(&a[0], a[1].clone())?
            }
            "actor_remote_ask" => {
                if a.len() < 2 || a.len() > 3 {
                    return Err("actor_remote_ask expects (target, message[, timeout_ms])".into());
                }
                let timeout = parse_nonneg_timeout_ms(a.get(2), "actor_remote_ask")?;
                crate::sim_actor::actor_remote_ask(&a[0], a[1].clone(), timeout)?
            }
            "actor_spawn" => {
                if a.is_empty() || a.len() > 2 {
                    return Err("actor_spawn expects (handler[, options_map])".into());
                }
                crate::sim_actor::actor_spawn(self, a[0].clone(), a.get(1))?
            }
            "actor_send" => {
                if a.len() != 2 {
                    return Err("actor_send expects (actor_id, message)".into());
                }
                let Val::Num(id) = a[0] else {
                    return Err("actor_send id must be Number".into());
                };
                if !id.is_finite() || id < 0.0 {
                    return Err("actor_send id must be non-negative Number".into());
                }
                crate::sim_actor::actor_send(id as u64, a[1].clone())?
            }
            "actor_ask" => {
                if a.len() < 2 || a.len() > 3 {
                    return Err("actor_ask expects (actor_id, message[, timeout_ms])".into());
                }
                let Val::Num(id) = a[0] else {
                    return Err("actor_ask id must be Number".into());
                };
                if !id.is_finite() || id < 0.0 {
                    return Err("actor_ask id must be non-negative Number".into());
                }
                let timeout = parse_nonneg_timeout_ms(a.get(2), "actor_ask")?;
                crate::sim_actor::actor_ask(id as u64, a[1].clone(), timeout)?
            }
            "actor_stop" => {
                if a.len() != 1 {
                    return Err("actor_stop expects (actor_id)".into());
                }
                let Val::Num(id) = a[0] else {
                    return Err("actor_stop id must be Number".into());
                };
                crate::sim_actor::actor_stop(id as u64)?
            }
            "actor_status" => {
                if a.len() != 1 {
                    return Err("actor_status expects (actor_id)".into());
                }
                let Val::Num(id) = a[0] else {
                    return Err("actor_status id must be Number".into());
                };
                crate::sim_actor::actor_status(id as u64)?
            }
            "actor_mailbox_len" => {
                if a.len() != 1 {
                    return Err("actor_mailbox_len expects (actor_id)".into());
                }
                let Val::Num(id) = a[0] else {
                    return Err("actor_mailbox_len id must be Number".into());
                };
                if !id.is_finite() || id < 0.0 {
                    return Err("actor_mailbox_len id must be non-negative Number".into());
                }
                crate::sim_actor::actor_mailbox_len(id as u64)?
            }
            "supervisor_spawn" => {
                if a.len() < 2 || a.len() > 3 {
                    return Err(
                        "supervisor_spawn expects (strategy, child_specs[, options])".into(),
                    );
                }
                let (Val::Str(strategy), Val::List(specs)) = (&a[0], &a[1]) else {
                    return Err(
                        "supervisor_spawn expects (strategy: String, child_specs: List[, options])"
                            .into(),
                    );
                };
                crate::sim_actor::supervisor_spawn(self, strategy, specs, a.get(2))?
            }
            "supervisor_status" => {
                if a.len() != 1 {
                    return Err("supervisor_status expects (supervisor_id)".into());
                }
                let Val::Num(id) = a[0] else {
                    return Err("supervisor_status id must be Number".into());
                };
                if !id.is_finite() || id < 0.0 {
                    return Err("supervisor_status id must be non-negative Number".into());
                }
                crate::sim_actor::supervisor_status(id as u64)?
            }
            "ffi_bindgen" => {
                if a.len() != 2 {
                    return Err("ffi_bindgen expects (header_source, lib_path)".into());
                }
                let (Val::Str(header_src), Val::Str(lib_path)) = (&a[0], &a[1]) else {
                    return Err("ffi_bindgen expects String arguments".into());
                };
                Val::Str(crate::plugin::ffi_bindgen(header_src, lib_path)?)
            }
            "mem_alloc" => {
                crate::runtime::require_cap("ffi")?;
                if a.len() != 1 {
                    return Err("mem_alloc expects (size)".into());
                }
                let Val::Num(sz) = a[0] else {
                    return Err("mem_alloc size must be Number".into());
                };
                if !sz.is_finite() || sz <= 0.0 {
                    return Err("mem_alloc size must be positive".into());
                }
                Val::Num(crate::plugin::mem_alloc(sz as usize)? as f64)
            }
            "mem_free" => {
                crate::runtime::require_cap("ffi")?;
                if a.len() != 1 {
                    return Err("mem_free expects (ptr)".into());
                }
                let Val::Num(ptr) = a[0] else {
                    return Err("mem_free ptr must be Number".into());
                };
                Val::Bool(crate::plugin::mem_free(ptr as usize)?)
            }
            "mem_read_u8" | "mem_read_i32" | "mem_read_i64" | "mem_read_f32" | "mem_read_f64"
            | "mem_read_cstr" => {
                crate::runtime::require_cap("ffi")?;
                if a.is_empty() || a.len() > 2 {
                    return Err(format!("{n} expects (ptr[, offset])"));
                }
                let Val::Num(ptr) = a[0] else {
                    return Err(format!("{n} ptr must be Number"));
                };
                let offset = match a.get(1) {
                    None => 0usize,
                    Some(Val::Num(o)) if o.is_finite() && *o >= 0.0 => *o as usize,
                    Some(_) => return Err(format!("{n} offset must be non-negative Number")),
                };
                let p = ptr as usize;
                match n {
                    "mem_read_u8" => Val::Num(f64::from(crate::plugin::mem_read_u8(p, offset)?)),
                    "mem_read_i32" => Val::Num(f64::from(crate::plugin::mem_read_i32(p, offset)?)),
                    "mem_read_i64" => Val::Num(crate::plugin::mem_read_i64(p, offset)? as f64),
                    "mem_read_f32" => Val::Num(f64::from(crate::plugin::mem_read_f32(p, offset)?)),
                    "mem_read_f64" => Val::Num(crate::plugin::mem_read_f64(p, offset)?),
                    "mem_read_cstr" => Val::Str(crate::plugin::mem_read_cstr(p, offset)?),
                    _ => unreachable!(),
                }
            }
            "mem_write_u8" | "mem_write_i32" | "mem_write_i64" | "mem_write_f32"
            | "mem_write_f64" => {
                crate::runtime::require_cap("ffi")?;
                if a.len() != 2 && a.len() != 3 {
                    return Err(format!("{n} expects (ptr, [offset, ]val)"));
                }
                let Val::Num(ptr) = a[0] else {
                    return Err(format!("{n} ptr must be Number"));
                };
                let (offset, val) = if a.len() == 2 {
                    let Val::Num(v) = a[1] else {
                        return Err(format!("{n} val must be Number"));
                    };
                    (0usize, v)
                } else {
                    let Val::Num(o) = a[1] else {
                        return Err(format!("{n} offset must be Number"));
                    };
                    if !o.is_finite() || o < 0.0 {
                        return Err(format!("{n} offset must be non-negative Number"));
                    }
                    let Val::Num(v) = a[2] else {
                        return Err(format!("{n} val must be Number"));
                    };
                    (o as usize, v)
                };
                let p = ptr as usize;
                match n {
                    "mem_write_u8" => crate::plugin::mem_write_u8(p, offset, val as u8)?,
                    "mem_write_i32" => crate::plugin::mem_write_i32(p, offset, val as i32)?,
                    "mem_write_i64" => crate::plugin::mem_write_i64(p, offset, val as i64)?,
                    "mem_write_f32" => crate::plugin::mem_write_f32(p, offset, val as f32)?,
                    "mem_write_f64" => crate::plugin::mem_write_f64(p, offset, val)?,
                    _ => unreachable!(),
                }
                Val::Null
            }
            "ffi_call" => {
                crate::runtime::require_cap("ffi")?;
                if a.len() != 5 {
                    return Err(
                        "ffi_call expects (lib_path, symbol, ret_type, param_types, args)".into(),
                    );
                }
                let (
                    Val::Str(lib_path),
                    Val::Str(symbol),
                    Val::Str(ret_type),
                    Val::List(param_list),
                    Val::List(arg_list),
                ) = (&a[0], &a[1], &a[2], &a[3], &a[4])
                else {
                    return Err("ffi_call expects (String, String, String, List, List)".into());
                };
                let param_types: Vec<String> = param_list
                    .iter()
                    .map(|v| match v {
                        Val::Str(s) => Ok(s.clone()),
                        _ => Err("ffi_call param_types must be Strings".to_string()),
                    })
                    .collect::<Result<_, _>>()?;
                crate::plugin::ffi_call(lib_path, symbol, ret_type, &param_types, arg_list)?
            }
            _ => return Ok(None),
        };
        Ok(Some(v))
    }
}

fn parse_nonneg_timeout_ms(arg: Option<&Val>, fn_name: &str) -> Result<u64, String> {
    match arg {
        None | Some(Val::Null) => Ok(10_000),
        Some(Val::Num(ms)) if ms.is_finite() && *ms >= 0.0 => Ok(*ms as u64),
        Some(_) => Err(format!("{fn_name} timeout_ms must be non-negative Number")),
    }
}
