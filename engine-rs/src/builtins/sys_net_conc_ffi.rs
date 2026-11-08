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
            #[cfg(feature = "native-services")]
            "http_request" => {
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
                let (name, def) = self.callable_def(&a[0])?;
                let args = match &a[1] {
                    Val::List(v) => v.clone(),
                    _ => return Err("spawn args must be List".into()),
                };
                if def.params.len() != args.len() {
                    return Err(format!(
                        "{name} expects {} arguments, got {}",
                        def.params.len(),
                        args.len()
                    ));
                }
                for ((param, ty), value) in def.params.iter().zip(args.iter()) {
                    if let Some(expected) = ty {
                        if !self.value_matches(value, expected) {
                            return Err(format!(
                                "argument '{param}' expected {expected}, got {}",
                                value.show()
                            ));
                        }
                    }
                }
                let funcs = self.funcs.clone();
                let classes = self.classes.clone();
                let objects = self.objects.clone();
                let id = TASK_IDS.fetch_add(1, Ordering::Relaxed);
                let handle = thread::spawn(move || {
                    let mut vars = (*def.captures).clone();
                    let mut cells = def.upvalues.clone();
                    let mut var_types = def.capture_types.clone();
                    let return_type = def.return_type.clone();
                    for ((param, ty), value) in def.params.iter().zip(args.into_iter()) {
                        if let Some(expected) = ty {
                            Arc::make_mut(&mut var_types).insert(param.clone(), expected.clone());
                        }
                        if cells.contains_key(param) {
                            Arc::make_mut(&mut cells).remove(param);
                        }
                        vars.insert(param.clone(), value);
                    }
                    let mut nested = Vm::nested(def.code, vars, funcs, classes, objects);
                    nested.var_types = var_types;
                    nested.cells = cells;
                    let result = nested.execute()?;
                    if let Some(expected) = return_type {
                        if !nested.value_matches(&result, &expected) {
                            return Err(format!(
                                "task return value expected {expected}, got {}",
                                result.show()
                            ));
                        }
                    }
                    Ok(result)
                });
                tasks()
                    .lock()
                    .map_err(|_| "task registry poisoned")?
                    .insert(id, handle);
                Val::Num(id as f64)
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
            _ => return Ok(None),
        };
        Ok(Some(v))
    }
}
