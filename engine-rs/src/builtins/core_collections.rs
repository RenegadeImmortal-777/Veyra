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
use crate::pipeline::{compile_source, compile_to_vybc, load_source, run_vybc};
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
    pub(crate) fn call_builtin_core_collections(
        &mut self,
        n: &str,
        a: Vec<Val>,
    ) -> Result<Option<Val>, String> {
        let v = match n {
            "print" => self.print_val(&a)?,
            "vybc_compile" => {
                if a.len() != 1 {
                    return Err("vybc_compile expects source String".into());
                }
                let src = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("vybc_compile expects String".into()),
                };
                Val::Str(compile_to_vybc(src)?)
            }
            "vybc_run" => {
                if a.len() != 1 {
                    return Err("vybc_run expects vybc String".into());
                }
                let bc = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("vybc_run expects String".into()),
                };
                run_vybc(bc)?;
                Val::Null
            }
            "import_package" => {
                if a.len() != 1 {
                    return Err("import_package expects package name".into());
                }
                let package = match &a[0] {
                    Val::Str(s) => s.clone(),
                    _ => return Err("package name must be String".into()),
                };
                let cwd = env::current_dir().map_err(|e| e.to_string())?;
                let root = project_root(&cwd)?;
                let manifest: JsonValue = serde_json::from_str(
                    &fs::read_to_string(root.join("veyra.json")).map_err(|e| e.to_string())?,
                )
                .map_err(|e| e.to_string())?;
                let meta = manifest
                    .get("packages")
                    .and_then(|p| p.get(&package))
                    .ok_or(format!("package '{package}' is not installed"))?;
                let entry = meta
                    .get("entry")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("main.vy");
                let path = root.join("vendor").join(&package).join(entry);
                let source = load_source(path.to_string_lossy().as_ref())?;
                let mut module_vm = Vm::new(compile_source(&source)?);
                module_vm.objects = self.objects.clone();
                module_vm.run()?;
                for (class_name, class_def) in module_vm.classes.iter() {
                    Arc::make_mut(&mut self.classes)
                        .entry(class_name.clone())
                        .or_insert_with(|| class_def.clone());
                }
                let local_names: HashSet<String> = module_vm.funcs.keys().cloned().collect();
                let module_id = FUNCTION_IDS.fetch_add(1, Ordering::Relaxed);
                let prefix = format!("@module:{package}:{module_id}:");
                let mut exports = vec![];
                for (name, def_ref) in module_vm.funcs.iter() {
                    let mut def = def_ref.clone();
                    let code_vec: &mut Vec<Op> = Arc::make_mut(&mut def.code);
                    namespace_module_ops(code_vec, &local_names, &prefix);
                    if !def.captures.is_empty() {
                        for value in Arc::make_mut(&mut def.captures).values_mut() {
                            if let Val::Function(target) = value {
                                if local_names.contains(target) {
                                    *target = format!("{prefix}{target}");
                                }
                            }
                        }
                    }
                    for cell in def.upvalues.values() {
                        if let Ok(mut guard) = cell.lock() {
                            if let Val::Function(target) = &mut *guard {
                                if local_names.contains(target) {
                                    *target = format!("{prefix}{target}");
                                }
                            }
                        }
                    }
                    let qualified = format!("{prefix}{name}");
                    register_named_function_object(&qualified, def.clone())?;
                    Arc::make_mut(&mut self.funcs).insert(qualified.clone(), def);
                    exports.push((Val::Str(name.clone()), Val::Function(qualified)));
                }
                for (name, value) in module_vm.vars {
                    if !name.starts_with("__") {
                        exports.push((Val::Str(name), value));
                    }
                }
                Val::Map(exports)
            }
            "len" => self.len_val(&a)?,
            "str" => {
                if a.len() != 1 {
                    return Err("str expects 1 argument".into());
                }
                Val::Str(self.show_val_runtime(&a[0])?)
            }
            "repr" => {
                if a.len() != 1 {
                    return Err("repr expects 1 argument".into());
                }
                Val::Str(self.repr_val_runtime(&a[0])?)
            }
            "callable" => {
                if a.len() != 1 {
                    return Err("callable expects 1 argument".into());
                }
                let ok = match &a[0] {
                    Val::Function(_) => true,
                    Val::Map(_) if crate::py_bridge::is_py_handle(&a[0]) => {
                        let built = crate::py_bridge::py_import("builtins")?;
                        crate::py_bridge::py_call_method(
                            &built,
                            "callable",
                            std::slice::from_ref(&a[0]),
                            None,
                        )?
                        .truthy()
                    }
                    Val::Object(id) => {
                        let cls = self
                            .objects
                            .lock()
                            .map_err(|_| "object heap poisoned")?
                            .get(id)
                            .map(|o| o.class_name.clone());
                        cls.and_then(|c| self.find_method_cached(&c, "__call__"))
                            .is_some()
                    }
                    _ => false,
                };
                Val::Bool(ok)
            }
            "divmod" => {
                if a.len() != 2 {
                    return Err("divmod expects 2 arguments".into());
                }
                let (Val::Num(x), Val::Num(y)) = (&a[0], &a[1]) else {
                    return Err("divmod expects Numbers".into());
                };
                if *y == 0.0 {
                    return Err("division by zero in divmod".into());
                }
                let q = (x / y).floor();
                let r = x - y * q;
                Val::List(vec![Val::Num(q), Val::Num(r)])
            }
            "getattr" => {
                if a.len() < 2 || a.len() > 3 {
                    return Err("getattr expects (obj, name[, default])".into());
                }
                let attr = match &a[1] {
                    Val::Str(s) => s,
                    _ => return Err("getattr attribute name must be String".into()),
                };
                if crate::py_bridge::is_py_handle(&a[0]) {
                    match crate::py_bridge::py_get(&a[0], attr) {
                        Ok(v) => v,
                        Err(e) => {
                            if a.len() == 3 {
                                a[2].clone()
                            } else {
                                return Err(e);
                            }
                        }
                    }
                } else {
                    match &a[0] {
                        Val::Object(id) => {
                            let val = self
                                .objects
                                .lock()
                                .map_err(|_| "object heap poisoned")?
                                .get(id)
                                .and_then(|o| o.fields.get(attr).cloned());
                            match val {
                                Some(v) => v,
                                None if a.len() == 3 => a[2].clone(),
                                None => return Err(format!("undefined field '{attr}'")),
                            }
                        }
                        Val::Map(pairs) => {
                            if let Some((_, v)) =
                                pairs.iter().find(|(k, _)| *k == Val::Str(attr.clone()))
                            {
                                v.clone()
                            } else if a.len() == 3 {
                                a[2].clone()
                            } else {
                                return Err(format!("undefined attribute '{attr}'"));
                            }
                        }
                        _ => {
                            if a.len() == 3 {
                                a[2].clone()
                            } else {
                                return Err("getattr expects Object, Map, or Python object".into());
                            }
                        }
                    }
                }
            }
            "hasattr" => {
                if a.len() != 2 {
                    return Err("hasattr expects (obj, name)".into());
                }
                let attr = match &a[1] {
                    Val::Str(s) => s,
                    _ => return Err("hasattr attribute name must be String".into()),
                };
                if crate::py_bridge::is_py_handle(&a[0]) {
                    Val::Bool(crate::py_bridge::py_hasattr(&a[0], attr)?)
                } else {
                    let yes = match &a[0] {
                        Val::Object(id) => {
                            let (has_field, cls) = {
                                let objs =
                                    self.objects.lock().map_err(|_| "object heap poisoned")?;
                                objs.get(id)
                                    .map(|o| {
                                        (o.fields.contains_key(attr), Some(o.class_name.clone()))
                                    })
                                    .unwrap_or((false, None))
                            };
                            has_field
                                || cls
                                    .and_then(|c| self.find_method_cached(&c, attr))
                                    .is_some()
                        }
                        Val::Map(pairs) => pairs.iter().any(|(k, _)| *k == Val::Str(attr.clone())),
                        _ => false,
                    };
                    Val::Bool(yes)
                }
            }
            "setattr" => {
                if a.len() != 3 {
                    return Err("setattr expects (obj, name, value)".into());
                }
                let attr = match &a[1] {
                    Val::Str(s) => s.clone(),
                    _ => return Err("setattr attribute name must be String".into()),
                };
                if crate::py_bridge::is_py_handle(&a[0]) {
                    let _ = crate::py_bridge::py_set(&a[0], &attr, &a[2])?;
                    Val::Null
                } else if let Val::Object(id) = a[0] {
                    let mut heap = self.objects.lock().map_err(|_| "object heap poisoned")?;
                    let data = heap.get_mut(&id).ok_or("dangling object")?;
                    data.fields.insert(attr, a[2].clone());
                    Val::Null
                } else {
                    return Err("setattr expects Object or Python object".into());
                }
            }
            "type_of" => self.type_of_val(&a)?,
            "is_instance" | "isinstance" => {
                if a.len() != 2 {
                    return Err("is_instance expects value,type".into());
                }
                let ty = match &a[1] {
                    Val::Str(s) => s,
                    _ => return Err("type must be String".into()),
                };
                Val::Bool(self.value_matches(&a[0], ty))
            }
            "has_field" => {
                if a.len() != 2 {
                    return Err("has_field expects object,name".into());
                }
                let id = match a[0] {
                    Val::Object(id) => id,
                    _ => return Err("has_field expects Object".into()),
                };
                let name = match &a[1] {
                    Val::Str(s) => s,
                    _ => return Err("field name must be String".into()),
                };
                let heap = self.objects.lock().map_err(|_| "object heap poisoned")?;
                Val::Bool(
                    heap.get(&id)
                        .map(|o| o.fields.contains_key(name))
                        .unwrap_or(false),
                )
            }
            "int" => {
                if a.len() != 1 {
                    return Err("int expects 1 argument".into());
                }
                Val::Num(match &a[0] {
                    Val::Num(n) => n.trunc(),
                    Val::Bool(b) => {
                        if *b {
                            1.0
                        } else {
                            0.0
                        }
                    }
                    Val::Str(s) => s
                        .trim()
                        .parse::<f64>()
                        .map_err(|_| "int could not parse String")?
                        .trunc(),
                    _ => return Err("int supports Number/Bool/String".into()),
                })
            }
            "float" => {
                if a.len() != 1 {
                    return Err("float expects 1 argument".into());
                }
                Val::Num(match &a[0] {
                    Val::Num(n) => *n,
                    Val::Bool(b) => {
                        if *b {
                            1.0
                        } else {
                            0.0
                        }
                    }
                    Val::Str(s) => s
                        .trim()
                        .parse::<f64>()
                        .map_err(|_| "float could not parse String")?,
                    _ => return Err("float supports Number/Bool/String".into()),
                })
            }
            "bool" => {
                if a.len() != 1 {
                    return Err("bool expects 1 argument".into());
                }
                Val::Bool(a[0].truthy())
            }
            "assert" => {
                if a.is_empty() || a.len() > 2 {
                    return Err("assert expects condition[,message]".into());
                }
                if !a[0].truthy() {
                    let message = if a.len() == 2 {
                        a[1].show()
                    } else {
                        "assertion failed".into()
                    };
                    return Err(message);
                }
                Val::Null
            }
            "sum" => {
                if a.len() != 1 {
                    return Err("sum expects iterable".into());
                }
                let items = self.iterable_values(&a[0])?;
                let mut total = 0.0;
                for x in items {
                    let Val::Num(n) = x else {
                        return Err("sum expects numeric items".into());
                    };
                    total += n;
                }
                Val::Num(total)
            }
            "min" => {
                if a.len() != 1 {
                    return Err("min expects iterable".into());
                }
                let items = self.iterable_values(&a[0])?;
                let mut best: Option<f64> = None;
                for x in items {
                    let Val::Num(n) = x else {
                        return Err("min expects numeric items".into());
                    };
                    best = Some(best.map_or(n, |b| b.min(n)));
                }
                Val::Num(best.ok_or("min requires non-empty iterable")?)
            }
            "max" => {
                if a.len() != 1 {
                    return Err("max expects iterable".into());
                }
                let items = self.iterable_values(&a[0])?;
                let mut best: Option<f64> = None;
                for x in items {
                    let Val::Num(n) = x else {
                        return Err("max expects numeric items".into());
                    };
                    best = Some(best.map_or(n, |b| b.max(n)));
                }
                Val::Num(best.ok_or("max requires non-empty iterable")?)
            }
            "all" => {
                if a.len() != 1 {
                    return Err("all expects iterable".into());
                }
                let xs = self.iterable_values(&a[0])?;
                Val::Bool(xs.iter().all(Val::truthy))
            }
            "any" => {
                if a.len() != 1 {
                    return Err("any expects iterable".into());
                }
                let xs = self.iterable_values(&a[0])?;
                Val::Bool(xs.iter().any(Val::truthy))
            }
            "__list" => Val::List(a),
            "range" => {
                if a.is_empty() || a.len() > 3 {
                    return Err("range expects (end), (start,end), or (start,end,step)".into());
                }
                let (start, end, step) = match a.len() {
                    1 => match a[0] {
                        Val::Num(end) => (0.0, end, 1.0),
                        _ => return Err("range arguments must be Numbers".into()),
                    },
                    2 => match (&a[0], &a[1]) {
                        (Val::Num(s), Val::Num(e)) => (*s, *e, 1.0),
                        _ => return Err("range arguments must be Numbers".into()),
                    },
                    _ => match (&a[0], &a[1], &a[2]) {
                        (Val::Num(s), Val::Num(e), Val::Num(st)) => (*s, *e, *st),
                        _ => return Err("range arguments must be Numbers".into()),
                    },
                };
                if step == 0.0 {
                    return Err("range step cannot be zero".into());
                }
                let mut out = vec![];
                let mut cur = start;
                if step > 0.0 {
                    while cur < end {
                        out.push(Val::Num(cur));
                        cur += step;
                    }
                } else {
                    while cur > end {
                        out.push(Val::Num(cur));
                        cur += step;
                    }
                }
                Val::List(out)
            }
            "next" => {
                if a.len() != 1 {
                    return Err("next expects generator".into());
                }
                let id = match a[0] {
                    Val::Generator(id) => id,
                    _ => return Err("next expects Generator".into()),
                };
                resume_generator(id)?.unwrap_or(Val::Null)
            }
            "generator_done" => {
                if a.len() != 1 {
                    return Err("generator_done expects generator".into());
                }
                let id = match a[0] {
                    Val::Generator(id) => id,
                    _ => return Err("generator_done expects Generator".into()),
                };
                Val::Bool(
                    !generators()
                        .lock()
                        .map_err(|_| "generator registry poisoned")?
                        .contains_key(&id),
                )
            }
            "collect" => {
                if a.len() != 1 {
                    return Err("collect expects iterable".into());
                }
                match &a[0] {
                    Val::Generator(id) => Val::List(collect_generator(*id)?),
                    Val::List(v) => Val::List(v.clone()),
                    Val::Str(s) => Val::List(s.chars().map(|c| Val::Str(c.to_string())).collect()),
                    Val::Map(pairs) => Val::List(pairs.iter().map(|(k, _)| k.clone()).collect()),
                    _ => return Err("collect expects Generator/List/String/Map".into()),
                }
            }
            "__iter_list" => {
                if a.len() != 1 {
                    return Err("__iter_list expects 1 argument".into());
                }
                match &a[0] {
                    Val::List(v) => Val::List(v.clone()),
                    Val::Str(s) => Val::List(s.chars().map(|c| Val::Str(c.to_string())).collect()),
                    Val::Map(pairs) => Val::List(pairs.iter().map(|(k, _)| k.clone()).collect()),
                    Val::Generator(id) => Val::List(collect_generator(*id)?),
                    _ => return Err("value is not iterable".into()),
                }
            }
            "__map" => {
                if !a.len().is_multiple_of(2) {
                    return Err("internal map arity error".into());
                }
                let mut pairs = vec![];
                let mut it = a.into_iter();
                while let Some(k) = it.next() {
                    let v = it.next().ok_or("internal map value missing")?;
                    pairs.push((k, v));
                }
                Val::Map(pairs)
            }
            "get" => {
                if a.len() != 2 {
                    return Err("get expects map and key".into());
                }
                match &a[0] {
                    Val::Map(pairs) => pairs
                        .iter()
                        .find(|(k, _)| *k == a[1])
                        .map(|(_, v)| v.clone())
                        .unwrap_or(Val::Null),
                    _ => return Err("get expects a map".into()),
                }
            }
            "keys" => {
                if a.len() != 1 {
                    return Err("keys expects a map".into());
                }
                match &a[0] {
                    Val::Map(pairs) => Val::List(pairs.iter().map(|(k, _)| k.clone()).collect()),
                    _ => return Err("keys expects a map".into()),
                }
            }
            "values" => {
                if a.len() != 1 {
                    return Err("values expects a map".into());
                }
                match &a[0] {
                    Val::Map(pairs) => Val::List(pairs.iter().map(|(_, v)| v.clone()).collect()),
                    _ => return Err("values expects a map".into()),
                }
            }
            "tuple" => {
                if a.len() != 1 {
                    return Err("tuple expects list".into());
                }
                match &a[0] {
                    Val::List(v) => Val::List(v.clone()),
                    _ => return Err("tuple expects List".into()),
                }
            }
            "list" => {
                if a.is_empty() {
                    Val::List(vec![])
                } else if a.len() == 1 {
                    match &a[0] {
                        Val::List(v) => Val::List(v.clone()),
                        Val::Str(s) => {
                            Val::List(s.chars().map(|c| Val::Str(c.to_string())).collect())
                        }
                        Val::Map(pairs) => {
                            Val::List(pairs.iter().map(|(k, _)| k.clone()).collect())
                        }
                        Val::Generator(id) => Val::List(collect_generator(*id)?),
                        _ => return Err("list expects iterable".into()),
                    }
                } else {
                    return Err("list expects at most 1 argument".into());
                }
            }
            "dict" => {
                if a.is_empty() {
                    Val::Map(vec![])
                } else if a.len() == 1 {
                    match &a[0] {
                        Val::Map(pairs) => Val::Map(pairs.clone()),
                        Val::List(items) => {
                            let mut pairs: Vec<(Val, Val)> = Vec::with_capacity(items.len());
                            for item in items {
                                match item {
                                    Val::List(kv) if kv.len() == 2 => {
                                        if let Some((_, existing)) =
                                            pairs.iter_mut().find(|(ek, _)| *ek == kv[0])
                                        {
                                            *existing = kv[1].clone();
                                        } else {
                                            pairs.push((kv[0].clone(), kv[1].clone()));
                                        }
                                    }
                                    _ => {
                                        return Err(
                                            "dict expects Map or List of [key, value] pairs".into(),
                                        );
                                    }
                                }
                            }
                            Val::Map(pairs)
                        }
                        _ => return Err("dict expects Map or List of pairs".into()),
                    }
                } else {
                    return Err("dict expects at most 1 argument".into());
                }
            }
            "set" => {
                if a.len() != 1 {
                    return Err("set expects list".into());
                }
                let xs = match &a[0] {
                    Val::List(v) => v,
                    _ => return Err("set expects List".into()),
                };
                let mut out = vec![];
                for x in xs {
                    if !out.contains(x) {
                        out.push(x.clone());
                    }
                }
                Val::List(out)
            }
            "set_union" => {
                if a.len() != 2 {
                    return Err("set_union expects two lists".into());
                }
                let x = match &a[0] {
                    Val::List(v) => v,
                    _ => return Err("set_union expects Lists".into()),
                };
                let y = match &a[1] {
                    Val::List(v) => v,
                    _ => return Err("set_union expects Lists".into()),
                };
                let mut out = vec![];
                for v in x.iter().chain(y.iter()) {
                    if !out.contains(v) {
                        out.push(v.clone());
                    }
                }
                Val::List(out)
            }
            "set_intersection" => {
                if a.len() != 2 {
                    return Err("set_intersection expects two lists".into());
                }
                let x = match &a[0] {
                    Val::List(v) => v,
                    _ => return Err("set_intersection expects Lists".into()),
                };
                let y = match &a[1] {
                    Val::List(v) => v,
                    _ => return Err("set_intersection expects Lists".into()),
                };
                let mut out = vec![];
                for v in x {
                    if y.contains(v) && !out.contains(v) {
                        out.push(v.clone());
                    }
                }
                Val::List(out)
            }
            "set_difference" => {
                if a.len() != 2 {
                    return Err("set_difference expects two lists".into());
                }
                let x = match &a[0] {
                    Val::List(v) => v,
                    _ => return Err("set_difference expects Lists".into()),
                };
                let y = match &a[1] {
                    Val::List(v) => v,
                    _ => return Err("set_difference expects Lists".into()),
                };
                Val::List(x.iter().filter(|v| !y.contains(v)).cloned().collect())
            }
            "contains" => {
                if a.len() != 2 {
                    return Err("contains expects collection,value".into());
                }
                Val::Bool(match &a[0] {
                    Val::List(v) => v.contains(&a[1]),
                    Val::Str(s) => match &a[1] {
                        Val::Str(x) => s.contains(x),
                        _ => false,
                    },
                    Val::Map(p) => p.iter().any(|(k, _)| *k == a[1]),
                    _ => return Err("contains supports List/String/Map".into()),
                })
            }
            "index_of" => {
                if a.len() != 2 {
                    return Err("index_of expects collection,value".into());
                }
                match &a[0] {
                    Val::List(v) => v
                        .iter()
                        .position(|x| x == &a[1])
                        .map(|i| Val::Num(i as f64))
                        .unwrap_or(Val::Null),
                    Val::Str(s) => match &a[1] {
                        Val::Str(x) => s.find(x).map(|i| Val::Num(i as f64)).unwrap_or(Val::Null),
                        _ => Val::Null,
                    },
                    _ => return Err("index_of supports List/String".into()),
                }
            }
            "append" => {
                if a.len() != 2 {
                    return Err("append expects list,value".into());
                }
                let mut xs = match &a[0] {
                    Val::List(v) => v.clone(),
                    _ => return Err("append expects List".into()),
                };
                xs.push(a[1].clone());
                Val::List(xs)
            }
            "concat" => {
                if a.len() != 2 {
                    return Err("concat expects two lists".into());
                }
                let mut x = match &a[0] {
                    Val::List(v) => v.clone(),
                    _ => return Err("concat expects Lists".into()),
                };
                let y = match &a[1] {
                    Val::List(v) => v.clone(),
                    _ => return Err("concat expects Lists".into()),
                };
                x.extend(y);
                Val::List(x)
            }
            "slice" => {
                if a.len() != 3 {
                    return Err("slice expects value,start,end".into());
                }
                let start = match a[1] {
                    Val::Num(n) => n.max(0.0) as usize,
                    _ => return Err("start must be Number".into()),
                };
                let end = match a[2] {
                    Val::Num(n) => n.max(0.0) as usize,
                    _ => return Err("end must be Number".into()),
                };
                match &a[0] {
                    Val::List(v) => Val::List(
                        v[start.min(v.len())..end.min(v.len()).max(start.min(v.len()))].to_vec(),
                    ),
                    Val::Str(s) => {
                        let chars: Vec<char> = s.chars().collect();
                        Val::Str(
                            chars[start.min(chars.len())
                                ..end.min(chars.len()).max(start.min(chars.len()))]
                                .iter()
                                .collect(),
                        )
                    }
                    _ => return Err("slice supports List/String".into()),
                }
            }
            "py_slice" => {
                if a.len() != 4 {
                    return Err("py_slice expects target,start,stop,step".into());
                }
                let step: isize = match &a[3] {
                    Val::Null => 1,
                    Val::Num(n) => {
                        let s = *n as isize;
                        if s == 0 {
                            return Err("slice step cannot be zero".into());
                        }
                        s
                    }
                    _ => return Err("slice step must be Number".into()),
                };
                let slice_indices = |len: isize| -> Result<Vec<usize>, String> {
                    let norm = |bound: &Val, default: isize| -> Result<isize, String> {
                        match bound {
                            Val::Null => Ok(default),
                            Val::Num(n) => {
                                let mut idx = *n as isize;
                                if idx < 0 {
                                    idx += len;
                                }
                                if step > 0 {
                                    Ok(idx.clamp(0, len))
                                } else {
                                    Ok(idx.clamp(-1, len - 1))
                                }
                            }
                            _ => Err("slice indices must be Numbers".into()),
                        }
                    };
                    let (start, stop) = if step > 0 {
                        (norm(&a[1], 0)?, norm(&a[2], len)?)
                    } else {
                        (norm(&a[1], len - 1)?, norm(&a[2], -1)?)
                    };
                    let mut indices = vec![];
                    let mut cur = start;
                    if step > 0 {
                        while cur < stop {
                            if cur >= 0 && cur < len {
                                indices.push(cur as usize);
                            }
                            cur += step;
                        }
                    } else {
                        while cur > stop {
                            if cur >= 0 && cur < len {
                                indices.push(cur as usize);
                            }
                            cur += step;
                        }
                    }
                    Ok(indices)
                };
                match &a[0] {
                    Val::List(v) => {
                        let idxs = slice_indices(v.len() as isize)?;
                        Val::List(idxs.into_iter().map(|i| v[i].clone()).collect())
                    }
                    Val::Str(s) => {
                        let chars: Vec<char> = s.chars().collect();
                        let idxs = slice_indices(chars.len() as isize)?;
                        Val::Str(idxs.into_iter().map(|i| chars[i]).collect())
                    }
                    _ => return Err("slicing supports List or String".into()),
                }
            }
            "reverse" | "reversed" => {
                if a.len() != 1 {
                    return Err("reverse expects list or string".into());
                }
                match &a[0] {
                    Val::List(v) => {
                        let mut xs = v.clone();
                        xs.reverse();
                        Val::List(xs)
                    }
                    Val::Str(s) => Val::Str(s.chars().rev().collect()),
                    _ => return Err("reverse expects List or String".into()),
                }
            }
            "sort" | "sorted" => {
                if a.is_empty() || a.len() > 2 {
                    return Err("sort expects (iterable[, reverse])".into());
                }
                let xs = self.iterable_values(&a[0])?;
                let reverse = a.get(1).map(Val::truthy).unwrap_or(false);
                Val::List(self.sorted_with_opts(xs, None, reverse)?)
            }
            "chr" => {
                if a.len() != 1 {
                    return Err("chr expects 1 argument".into());
                }
                match a[0] {
                    Val::Num(n) => {
                        let code = n as u32;
                        let ch = char::from_u32(code).ok_or("chr code point out of range")?;
                        Val::Str(ch.to_string())
                    }
                    _ => return Err("chr expects Number".into()),
                }
            }
            "ord" => {
                if a.len() != 1 {
                    return Err("ord expects 1 argument".into());
                }
                match &a[0] {
                    Val::Str(s) => {
                        let mut chars = s.chars();
                        let ch = chars.next().ok_or("ord expects non-empty String")?;
                        if chars.next().is_some() {
                            return Err("ord expects single-character String".into());
                        }
                        Val::Num((ch as u32) as f64)
                    }
                    _ => return Err("ord expects String".into()),
                }
            }
            "hex" => {
                if a.len() != 1 {
                    return Err("hex expects 1 argument".into());
                }
                match a[0] {
                    Val::Num(n) => Val::Str(format!("0x{:x}", n as i64)),
                    _ => return Err("hex expects Number".into()),
                }
            }
            "bin" => {
                if a.len() != 1 {
                    return Err("bin expects 1 argument".into());
                }
                match a[0] {
                    Val::Num(n) => Val::Str(format!("0b{:b}", n as i64)),
                    _ => return Err("bin expects Number".into()),
                }
            }
            "unique" => {
                if a.len() != 1 {
                    return Err("unique expects list".into());
                }
                let xs = match &a[0] {
                    Val::List(v) => v,
                    _ => return Err("unique expects List".into()),
                };
                let mut out = vec![];
                for x in xs {
                    if !out.contains(x) {
                        out.push(x.clone());
                    }
                }
                Val::List(out)
            }
            "enumerate" => {
                if a.is_empty() || a.len() > 2 {
                    return Err("enumerate expects (iterable[, start])".into());
                }
                let start = match a.get(1) {
                    Some(Val::Num(n)) => *n as i64,
                    None => 0,
                    _ => return Err("enumerate start must be Number".into()),
                };
                let xs = self.iterable_values(&a[0])?;
                Val::List(
                    xs.into_iter()
                        .enumerate()
                        .map(|(i, v)| Val::List(vec![Val::Num((start + i as i64) as f64), v]))
                        .collect(),
                )
            }
            "zip" => {
                if a.is_empty() {
                    Val::List(vec![])
                } else {
                    let mut cols = Vec::with_capacity(a.len());
                    for item in &a {
                        cols.push(self.iterable_values(item)?);
                    }
                    let min_len = cols.iter().map(Vec::len).min().unwrap_or(0);
                    let mut out = Vec::with_capacity(min_len);
                    for i in 0..min_len {
                        out.push(Val::List(cols.iter().map(|c| c[i].clone()).collect()));
                    }
                    Val::List(out)
                }
            }
            "map_call" => {
                if a.len() != 2 {
                    return Err("map_call expects callable,iterable".into());
                }
                let xs = self.iterable_values(&a[1])?;
                let mut out = vec![];
                for x in xs {
                    out.push(self.invoke_callable_sync(&a[0], vec![x])?);
                }
                Val::List(out)
            }
            "filter_call" => {
                if a.len() != 2 {
                    return Err("filter_call expects callable,iterable".into());
                }
                let xs = self.iterable_values(&a[1])?;
                let mut out = vec![];
                for x in xs {
                    if self.invoke_callable_sync(&a[0], vec![x.clone()])?.truthy() {
                        out.push(x);
                    }
                }
                Val::List(out)
            }
            "reduce_call" => {
                if a.len() != 3 {
                    return Err("reduce_call expects callable,iterable,initial".into());
                }
                let xs = self.iterable_values(&a[1])?;
                let mut acc = a[2].clone();
                for x in xs {
                    acc = self.invoke_callable_sync(&a[0], vec![acc, x])?;
                }
                acc
            }
            _ => return Ok(None),
        };
        Ok(Some(v))
    }
}
