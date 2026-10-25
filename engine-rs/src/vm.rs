// The stack virtual machine: executes the compiled `Op` stream.
//
// Extracted from `main.rs` without behavior change; see docs/ENGINE-SPLIT.md.
// `execute()` is still one function with one giant match; separating the 212
// inline builtins out of `Op::Call` is Phase 2 of the split.

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
use crate::project_root;
use crate::runtime::{
    CHANNEL_IDS, FUNCTION_IDS, GENERATOR_IDS, OBJECT_IDS, TASK_IDS, channels,
    export_function_object, namespace_module_ops, resolve_function_object, tasks,
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

pub(crate) struct Vm {
    code: Vec<Op>,
    ip: usize,
    stack: Vec<Val>,
    pub(crate) vars: HashMap<String, Val>,
    var_types: HashMap<String, String>,
    consts: HashSet<String>,
    pub(crate) funcs: HashMap<String, FunctionDef>,
    classes: HashMap<String, ClassDef>,
    objects: Arc<Mutex<HashMap<u64, ObjectData>>>,
    handlers: Vec<(usize, String)>,
    pub(crate) trace: bool,
    finished: bool,
}

static GENERATORS: OnceLock<Mutex<HashMap<u64, Vm>>> = OnceLock::new();

fn generators() -> &'static Mutex<HashMap<u64, Vm>> {
    GENERATORS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn resume_generator(id: u64) -> Result<Option<Val>, String> {
    let mut vm = generators()
        .lock()
        .map_err(|_| "generator registry poisoned")?
        .remove(&id)
        .ok_or(format!("unknown or completed generator {id}"))?;
    if vm.finished {
        return Ok(None);
    }
    let value = vm.execute()?;
    if vm.finished {
        Ok(None)
    } else {
        generators()
            .lock()
            .map_err(|_| "generator registry poisoned")?
            .insert(id, vm);
        Ok(Some(value))
    }
}

pub(crate) fn collect_generator(id: u64) -> Result<Vec<Val>, String> {
    let mut out = vec![];
    loop {
        match resume_generator(id) {
            Ok(Some(v)) => out.push(v),
            Ok(None) => break,
            Err(e) if e.starts_with("unknown or completed generator") => break,
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

impl Vm {
    pub(crate) fn new(code: Vec<Op>) -> Self {
        Self {
            code,
            ip: 0,
            stack: vec![],
            vars: HashMap::new(),
            var_types: HashMap::new(),
            consts: HashSet::new(),
            funcs: HashMap::new(),
            classes: HashMap::new(),
            objects: Arc::new(Mutex::new(HashMap::new())),
            handlers: vec![],
            trace: false,
            finished: false,
        }
    }
    fn nested(
        code: Vec<Op>,
        vars: HashMap<String, Val>,
        funcs: HashMap<String, FunctionDef>,
        classes: HashMap<String, ClassDef>,
        objects: Arc<Mutex<HashMap<u64, ObjectData>>>,
    ) -> Self {
        Self {
            code,
            ip: 0,
            stack: vec![],
            vars,
            var_types: HashMap::new(),
            consts: HashSet::new(),
            funcs,
            classes,
            objects,
            handlers: vec![],
            trace: false,
            finished: false,
        }
    }
    fn pop(&mut self) -> Result<Val, String> {
        self.stack.pop().ok_or("stack underflow".into())
    }
    fn bin_num<F: Fn(f64, f64) -> f64>(&mut self, f: F) -> Result<(), String> {
        let b = self.pop()?;
        let a = self.pop()?;
        if let (Val::Num(x), Val::Num(y)) = (a, b) {
            self.stack.push(Val::Num(f(x, y)));
            Ok(())
        } else {
            Err("numeric operands required".into())
        }
    }
    fn class_is_a(&self, class_name: &str, target: &str) -> bool {
        let mut current = Some(class_name.to_string());
        while let Some(name) = current {
            if name == target {
                return true;
            }
            current = self.classes.get(&name).and_then(|c| c.parent.clone());
        }
        false
    }
    fn value_matches(&self, value: &Val, ty: &str) -> bool {
        match ty {
            "Any" => true,
            "Number" | "Float" => matches!(value, Val::Num(_)),
            "Int" => matches!(value,Val::Num(n) if n.fract()==0.0),
            "String" => matches!(value, Val::Str(_)),
            "Bool" => matches!(value, Val::Bool(_)),
            "List" => matches!(value, Val::List(_)),
            "Map" => matches!(value, Val::Map(_)),
            "Null" | "Void" => matches!(value, Val::Null),
            "Object" => matches!(value, Val::Object(_)),
            "Function" => matches!(value, Val::Function(_)),
            "Generator" => matches!(value, Val::Generator(_)),
            custom => match value {
                Val::Object(id) => {
                    let Ok(heap) = self.objects.lock() else {
                        return false;
                    };
                    heap.get(id)
                        .map(|o| self.class_is_a(&o.class_name, custom))
                        .unwrap_or(false)
                }
                _ => false,
            },
        }
    }
    fn find_method(&self, class_name: &str, method: &str) -> Option<FunctionDef> {
        let mut current = Some(class_name.to_string());
        while let Some(name) = current {
            let class = self.classes.get(&name)?;
            if let Some(def) = class.methods.get(method) {
                return Some(def.clone());
            }
            current = class.parent.clone();
        }
        None
    }
    fn call_def(&self, def: FunctionDef, args: Vec<Val>) -> Result<Val, String> {
        if def.params.len() != args.len() {
            return Err(format!(
                "function expects {} arguments, got {}",
                def.params.len(),
                args.len()
            ));
        }
        let is_generator = def.code.iter().any(|op| matches!(op, Op::Yield));
        let is_async = def.is_async;
        if is_async && is_generator {
            return Err("async generator functions are not yet supported".into());
        }
        let mut vars = def.captures.clone();
        let mut var_types = def.capture_types.clone();
        let return_type = def.return_type.clone();
        for ((param, ty), value) in def.params.into_iter().zip(args.into_iter()) {
            if let Some(ref expected) = ty {
                if !self.value_matches(&value, expected) {
                    return Err(format!(
                        "argument '{param}' expected {expected}, got {}",
                        value.show()
                    ));
                }
                var_types.insert(param.clone(), expected.clone());
            }
            vars.insert(param, value);
        }
        let mut nested = Vm::nested(
            def.code,
            vars,
            self.funcs.clone(),
            self.classes.clone(),
            self.objects.clone(),
        );
        nested.var_types = var_types;
        if is_async {
            let id = TASK_IDS.fetch_add(1, Ordering::Relaxed);
            let expected_return = return_type.clone();
            let handle = thread::spawn(move || {
                let result = nested.execute()?;
                if let Some(expected) = expected_return {
                    if !nested.value_matches(&result, &expected) {
                        return Err(format!(
                            "async return value expected {expected}, got {}",
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
            return Ok(Val::Num(id as f64));
        }
        if is_generator {
            if let Some(expected) = return_type {
                if expected != "Generator" && expected != "Any" {
                    return Err(format!(
                        "generator function return type must be Generator or Any, got {expected}"
                    ));
                }
            }
            let id = GENERATOR_IDS.fetch_add(1, Ordering::Relaxed);
            generators()
                .lock()
                .map_err(|_| "generator registry poisoned")?
                .insert(id, nested);
            return Ok(Val::Generator(id));
        }
        let result = nested.execute()?;
        if let Some(expected) = return_type {
            if !self.value_matches(&result, &expected) {
                return Err(format!(
                    "return value expected {expected}, got {}",
                    result.show()
                ));
            }
        }
        Ok(result)
    }
    fn instantiate(&self, class_name: &str, args: Vec<Val>) -> Result<Val, String> {
        if !self.classes.contains_key(class_name) {
            return Err(format!("unknown class '{class_name}'"));
        }
        let id = OBJECT_IDS.fetch_add(1, Ordering::Relaxed);
        self.objects
            .lock()
            .map_err(|_| "object heap poisoned")?
            .insert(
                id,
                ObjectData {
                    class_name: class_name.to_string(),
                    fields: HashMap::new(),
                },
            );
        if let Some(init) = self.find_method(class_name, "init") {
            let mut call_args = vec![Val::Object(id)];
            call_args.extend(args);
            let _ = self.call_def(init, call_args)?;
        } else if !args.is_empty() {
            return Err(format!("{class_name} has no init method"));
        }
        Ok(Val::Object(id))
    }
    fn callable_def(&self, value: &Val) -> Result<(String, FunctionDef), String> {
        let name = match value {
            Val::Str(s) | Val::Function(s) => s.clone(),
            _ => return Err("callable must be a function value or function name String".into()),
        };
        let def = if let Some(def) = self.funcs.get(&name).cloned() {
            def
        } else if let Some(def) = resolve_function_object(&name)? {
            def
        } else {
            return Err(format!("unknown function '{name}'"));
        };
        Ok((name, def))
    }
    pub(crate) fn invoke(&self, name: &str, args: Vec<Val>) -> Result<Val, String> {
        let def = self
            .funcs
            .get(name)
            .cloned()
            .ok_or(format!("unknown function '{name}'"))?;
        self.call_def(def, args)
    }
    pub(crate) fn run(&mut self) -> Result<(), String> {
        self.execute().map(|_| ())
    }
    pub(crate) fn execute(&mut self) -> Result<Val, String> {
        loop {
            let op = self
                .code
                .get(self.ip)
                .cloned()
                .ok_or("instruction pointer out of range")?;
            if self.trace {
                let mut vars = self
                    .vars
                    .iter()
                    .map(|(k, v)| format!("{k}={}", v.show()))
                    .collect::<Vec<_>>();
                vars.sort();
                eprintln!(
                    "[veyra-debug pc={:04}] {:?} | stack=[{}] | vars={{ {} }}",
                    self.ip,
                    op,
                    self.stack
                        .iter()
                        .map(Val::show)
                        .collect::<Vec<_>>()
                        .join(", "),
                    vars.join(", ")
                );
            }
            self.ip += 1;
            match op {
                Op::Const(v) => self.stack.push(v),
                Op::Load(n) => {
                    let value = if let Some(v) = self.vars.get(&n) {
                        v.clone()
                    } else if self.funcs.contains_key(&n) {
                        Val::Function(n.clone())
                    } else {
                        return Err(format!("undefined name '{n}'"));
                    };
                    self.stack.push(value);
                }
                Op::Define(n, is_const, ty) => {
                    let v = self.pop()?;
                    if self.vars.contains_key(&n) && self.consts.contains(&n) {
                        return Err(format!("cannot redefine constant '{n}'"));
                    }
                    if self.vars.contains_key(&n) && is_const {
                        return Err(format!("name '{n}' already defined"));
                    }
                    if let Some(ref expected) = ty {
                        if !self.value_matches(&v, expected) {
                            return Err(format!(
                                "declaration '{n}' expected {expected}, got {}",
                                v.show()
                            ));
                        }
                        self.var_types.insert(n.clone(), expected.clone());
                    }
                    self.vars.insert(n.clone(), v);
                    if is_const {
                        self.consts.insert(n);
                    }
                }
                Op::Store(n) => {
                    if self.consts.contains(&n) {
                        return Err(format!("cannot assign to constant '{n}'"));
                    }
                    let v = self.pop()?;
                    if let Some(expected) = self.var_types.get(&n).cloned() {
                        if !self.value_matches(&v, &expected) {
                            return Err(format!(
                                "assignment '{n}' expected {expected}, got {}",
                                v.show()
                            ));
                        }
                    }
                    self.vars.insert(n, v);
                }
                Op::Pop => {
                    let _ = self.pop()?;
                }
                Op::Add => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.stack.push(match (a, b) {
                        (Val::Num(x), Val::Num(y)) => Val::Num(x + y),
                        (Val::Str(x), Val::Str(y)) => Val::Str(x + &y),
                        _ => return Err("invalid '+' operands".into()),
                    });
                }
                Op::Sub => self.bin_num(|a, b| a - b)?,
                Op::Mul => self.bin_num(|a, b| a * b)?,
                Op::Div => self.bin_num(|a, b| a / b)?,
                Op::Mod => self.bin_num(|a, b| a % b)?,
                Op::Eq => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.stack.push(Val::Bool(a == b));
                }
                Op::Ne => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.stack.push(Val::Bool(a != b));
                }
                Op::Lt | Op::Le | Op::Gt | Op::Ge => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    let (x, y) = match (a, b) {
                        (Val::Num(x), Val::Num(y)) => (x, y),
                        _ => return Err("comparison requires numbers".into()),
                    };
                    let yes = match op {
                        Op::Lt => x < y,
                        Op::Le => x <= y,
                        Op::Gt => x > y,
                        _ => x >= y,
                    };
                    self.stack.push(Val::Bool(yes));
                }
                Op::Neg => {
                    let a = self.pop()?;
                    if let Val::Num(x) = a {
                        self.stack.push(Val::Num(-x))
                    } else {
                        return Err("unary '-' requires number".into());
                    }
                }
                Op::Not => {
                    let a = self.pop()?;
                    self.stack.push(Val::Bool(!a.truthy()));
                }
                Op::And => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.stack.push(Val::Bool(a.truthy() && b.truthy()));
                }
                Op::Or => {
                    let b = self.pop()?;
                    let a = self.pop()?;
                    self.stack.push(Val::Bool(a.truthy() || b.truthy()));
                }
                Op::Index => {
                    let key = self.pop()?;
                    let obj = self.pop()?;
                    let value = match (obj, key) {
                        (Val::List(v), Val::Num(n)) => {
                            let i = n as usize;
                            v.get(i)
                                .cloned()
                                .ok_or(format!("list index {i} out of range"))?
                        }
                        (Val::Str(s), Val::Num(n)) => {
                            let i = n as usize;
                            s.chars()
                                .nth(i)
                                .map(|c| Val::Str(c.to_string()))
                                .ok_or(format!("string index {i} out of range"))?
                        }
                        (Val::Map(pairs), k) => pairs
                            .iter()
                            .find(|(key, _)| *key == k)
                            .map(|(_, v)| v.clone())
                            .ok_or("map key not found")?,
                        _ => return Err("value is not indexable".into()),
                    };
                    self.stack.push(value);
                }
                Op::GetMember(name) => {
                    let obj = self.pop()?;
                    let value = match obj {
                        Val::Object(id) => {
                            let heap = self.objects.lock().map_err(|_| "object heap poisoned")?;
                            let data = heap.get(&id).ok_or("dangling object")?;
                            data.fields
                                .get(&name)
                                .cloned()
                                .ok_or(format!("undefined field '{name}'"))?
                        }
                        Val::Map(pairs) => pairs
                            .iter()
                            .find(|(k, _)| *k == Val::Str(name.clone()))
                            .map(|(_, v)| v.clone())
                            .ok_or(format!("undefined key '{name}'"))?,
                        _ => return Err("member access requires object or map".into()),
                    };
                    self.stack.push(value);
                }
                Op::SetMember(name) => {
                    let value = self.pop()?;
                    let obj = self.pop()?;
                    match obj {
                        Val::Object(id) => {
                            let mut heap =
                                self.objects.lock().map_err(|_| "object heap poisoned")?;
                            let data = heap.get_mut(&id).ok_or("dangling object")?;
                            data.fields.insert(name, value);
                        }
                        _ => return Err("member assignment requires object".into()),
                    }
                }
                Op::CallMethod(name, k) => {
                    let mut args = vec![];
                    for _ in 0..k {
                        args.push(self.pop()?);
                    }
                    args.reverse();
                    let receiver = self.pop()?;
                    let value = match receiver {
                        Val::Object(id) => {
                            let class_name = {
                                let heap =
                                    self.objects.lock().map_err(|_| "object heap poisoned")?;
                                heap.get(&id).ok_or("dangling object")?.class_name.clone()
                            };
                            let method = self
                                .find_method(&class_name, &name)
                                .ok_or(format!("undefined method '{name}' on {class_name}"))?;
                            let mut call_args = vec![Val::Object(id)];
                            call_args.extend(args);
                            self.call_def(method, call_args)?
                        }
                        Val::Map(pairs) => {
                            let member = pairs
                                .iter()
                                .find(|(k, _)| *k == Val::Str(name.clone()))
                                .map(|(_, v)| v.clone())
                                .ok_or(format!("undefined module member '{name}'"))?;
                            let (_, def) = self.callable_def(&member)?;
                            self.call_def(def, args)?
                        }
                        _ => return Err("method call requires object or module map".into()),
                    };
                    self.stack.push(value);
                }
                Op::JumpIfFalse(p) => {
                    let a = self.pop()?;
                    if !a.truthy() {
                        self.ip = p
                    }
                }
                Op::Jump(p) => self.ip = p,
                Op::Call(n, k) => {
                    let mut a = vec![];
                    for _ in 0..k {
                        a.push(self.pop()?)
                    }
                    a.reverse();
                    let v = match n.as_str() {
                        "print" => {
                            println!("{}", a.iter().map(Val::show).collect::<Vec<_>>().join(" "));
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
                                &fs::read_to_string(root.join("veyra.json"))
                                    .map_err(|e| e.to_string())?,
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
                            module_vm.run()?;
                            let local_names: HashSet<String> =
                                module_vm.funcs.keys().cloned().collect();
                            let module_id = FUNCTION_IDS.fetch_add(1, Ordering::Relaxed);
                            let prefix = format!("@module:{package}:{module_id}:");
                            let mut exports = vec![];
                            for (name, mut def) in module_vm.funcs {
                                namespace_module_ops(&mut def.code, &local_names, &prefix);
                                for value in def.captures.values_mut() {
                                    if let Val::Function(target) = value {
                                        if local_names.contains(target) {
                                            *target = format!("{prefix}{target}");
                                        }
                                    }
                                }
                                let qualified = format!("{prefix}{name}");
                                self.funcs.insert(qualified.clone(), def);
                                exports.push((Val::Str(name), Val::Function(qualified)));
                            }
                            for (name, value) in module_vm.vars {
                                if !name.starts_with("__") {
                                    exports.push((Val::Str(name), value));
                                }
                            }
                            Val::Map(exports)
                        }
                        "len" => {
                            if a.len() != 1 {
                                return Err("len expects 1 argument".into());
                            }
                            Val::Num(match &a[0] {
                                Val::Str(s) => s.chars().count() as f64,
                                Val::List(v) => v.len() as f64,
                                Val::Map(v) => v.len() as f64,
                                _ => return Err("len supports String/List/Map".into()),
                            })
                        }
                        "str" => {
                            if a.len() != 1 {
                                return Err("str expects 1 argument".into());
                            }
                            Val::Str(a[0].show())
                        }
                        "type_of" => {
                            if a.len() != 1 {
                                return Err("type_of expects 1 argument".into());
                            }
                            let name = match &a[0] {
                                Val::Num(n) => {
                                    if n.fract() == 0.0 {
                                        "Int".to_string()
                                    } else {
                                        "Number".to_string()
                                    }
                                }
                                Val::Str(_) => "String".to_string(),
                                Val::Bool(_) => "Bool".to_string(),
                                Val::Null => "Null".to_string(),
                                Val::List(_) => "List".to_string(),
                                Val::Map(_) => "Map".to_string(),
                                Val::Function(_) => "Function".to_string(),
                                Val::Generator(_) => "Generator".to_string(),
                                Val::Object(id) => {
                                    let heap =
                                        self.objects.lock().map_err(|_| "object heap poisoned")?;
                                    heap.get(id)
                                        .map(|o| o.class_name.clone())
                                        .unwrap_or_else(|| "Object".into())
                                }
                            };
                            Val::Str(name)
                        }
                        "is_instance" => {
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
                                return Err("sum expects list".into());
                            }
                            Val::Num(as_num_list(&a[0])?.iter().sum())
                        }
                        "min" => {
                            if a.len() != 1 {
                                return Err("min expects list".into());
                            }
                            let xs = as_num_list(&a[0])?;
                            Val::Num(
                                xs.into_iter()
                                    .reduce(f64::min)
                                    .ok_or("min requires non-empty list")?,
                            )
                        }
                        "max" => {
                            if a.len() != 1 {
                                return Err("max expects list".into());
                            }
                            let xs = as_num_list(&a[0])?;
                            Val::Num(
                                xs.into_iter()
                                    .reduce(f64::max)
                                    .ok_or("max requires non-empty list")?,
                            )
                        }
                        "all" => {
                            if a.len() != 1 {
                                return Err("all expects list".into());
                            }
                            let xs = match &a[0] {
                                Val::List(v) => v,
                                _ => return Err("all expects List".into()),
                            };
                            Val::Bool(xs.iter().all(Val::truthy))
                        }
                        "any" => {
                            if a.len() != 1 {
                                return Err("any expects list".into());
                            }
                            let xs = match &a[0] {
                                Val::List(v) => v,
                                _ => return Err("any expects List".into()),
                            };
                            Val::Bool(xs.iter().any(Val::truthy))
                        }
                        "__list" => Val::List(a),
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
                                Val::Str(s) => {
                                    Val::List(s.chars().map(|c| Val::Str(c.to_string())).collect())
                                }
                                Val::Map(pairs) => {
                                    Val::List(pairs.iter().map(|(k, _)| k.clone()).collect())
                                }
                                _ => return Err("collect expects Generator/List/String/Map".into()),
                            }
                        }
                        "__iter_list" => {
                            if a.len() != 1 {
                                return Err("__iter_list expects 1 argument".into());
                            }
                            match &a[0] {
                                Val::List(v) => Val::List(v.clone()),
                                Val::Str(s) => {
                                    Val::List(s.chars().map(|c| Val::Str(c.to_string())).collect())
                                }
                                Val::Map(pairs) => {
                                    Val::List(pairs.iter().map(|(k, _)| k.clone()).collect())
                                }
                                Val::Generator(id) => Val::List(collect_generator(*id)?),
                                _ => return Err("value is not iterable".into()),
                            }
                        }
                        "__map" => {
                            if a.len() % 2 != 0 {
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
                                Val::Map(pairs) => {
                                    Val::List(pairs.iter().map(|(k, _)| k.clone()).collect())
                                }
                                _ => return Err("keys expects a map".into()),
                            }
                        }
                        "values" => {
                            if a.len() != 1 {
                                return Err("values expects a map".into());
                            }
                            match &a[0] {
                                Val::Map(pairs) => {
                                    Val::List(pairs.iter().map(|(_, v)| v.clone()).collect())
                                }
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
                                    Val::Str(x) => {
                                        s.find(x).map(|i| Val::Num(i as f64)).unwrap_or(Val::Null)
                                    }
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
                                    v[start.min(v.len())..end.min(v.len()).max(start.min(v.len()))]
                                        .to_vec(),
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
                        "reverse" => {
                            if a.len() != 1 {
                                return Err("reverse expects list".into());
                            }
                            let mut xs = match &a[0] {
                                Val::List(v) => v.clone(),
                                _ => return Err("reverse expects List".into()),
                            };
                            xs.reverse();
                            Val::List(xs)
                        }
                        "sort" => {
                            if a.len() != 1 {
                                return Err("sort expects list".into());
                            }
                            let mut xs = match &a[0] {
                                Val::List(v) => v.clone(),
                                _ => return Err("sort expects List".into()),
                            };
                            xs.sort_by(|x, y| match (x, y) {
                                (Val::Num(a), Val::Num(b)) => {
                                    a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)
                                }
                                (Val::Str(a), Val::Str(b)) => a.cmp(b),
                                _ => std::cmp::Ordering::Equal,
                            });
                            Val::List(xs)
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
                            if a.len() != 1 {
                                return Err("enumerate expects list".into());
                            }
                            let xs = match &a[0] {
                                Val::List(v) => v,
                                _ => return Err("enumerate expects List".into()),
                            };
                            Val::List(
                                xs.iter()
                                    .enumerate()
                                    .map(|(i, v)| Val::List(vec![Val::Num(i as f64), v.clone()]))
                                    .collect(),
                            )
                        }
                        "zip" => {
                            if a.len() != 2 {
                                return Err("zip expects two lists".into());
                            }
                            let x = match &a[0] {
                                Val::List(v) => v,
                                _ => return Err("zip expects Lists".into()),
                            };
                            let y = match &a[1] {
                                Val::List(v) => v,
                                _ => return Err("zip expects Lists".into()),
                            };
                            Val::List(
                                x.iter()
                                    .zip(y.iter())
                                    .map(|(a, b)| Val::List(vec![a.clone(), b.clone()]))
                                    .collect(),
                            )
                        }
                        "map_call" => {
                            if a.len() != 2 {
                                return Err("map_call expects callable,list".into());
                            }
                            let (_, def) = self.callable_def(&a[0])?;
                            let xs = match &a[1] {
                                Val::List(v) => v,
                                _ => return Err("map_call expects List".into()),
                            };
                            let mut out = vec![];
                            for x in xs {
                                out.push(self.call_def(def.clone(), vec![x.clone()])?);
                            }
                            Val::List(out)
                        }
                        "filter_call" => {
                            if a.len() != 2 {
                                return Err("filter_call expects callable,list".into());
                            }
                            let (_, def) = self.callable_def(&a[0])?;
                            let xs = match &a[1] {
                                Val::List(v) => v,
                                _ => return Err("filter_call expects List".into()),
                            };
                            let mut out = vec![];
                            for x in xs {
                                if self.call_def(def.clone(), vec![x.clone()])?.truthy() {
                                    out.push(x.clone());
                                }
                            }
                            Val::List(out)
                        }
                        "reduce_call" => {
                            if a.len() != 3 {
                                return Err("reduce_call expects callable,list,initial".into());
                            }
                            let (_, def) = self.callable_def(&a[0])?;
                            let xs = match &a[1] {
                                Val::List(v) => v,
                                _ => return Err("reduce_call expects List".into()),
                            };
                            let mut acc = a[2].clone();
                            for x in xs {
                                acc = self.call_def(def.clone(), vec![acc, x.clone()])?;
                            }
                            acc
                        }
                        "json_stringify" => {
                            if a.len() != 1 {
                                return Err("json_stringify expects 1 argument".into());
                            }
                            Val::Str(
                                serde_json::to_string(&val_to_json(&a[0]))
                                    .map_err(|e| e.to_string())?,
                            )
                        }
                        "json_parse" => {
                            if a.len() != 1 {
                                return Err("json_parse expects 1 argument".into());
                            }
                            let s = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("json_parse expects String".into()),
                            };
                            json_to_val(
                                &serde_json::from_str::<JsonValue>(s).map_err(|e| e.to_string())?,
                            )
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
                        "vec_sum" => {
                            if a.len() != 1 {
                                return Err("vec_sum expects list".into());
                            }
                            let xs = as_num_list(&a[0])?;
                            Val::Num(xs.iter().sum())
                        }
                        "vec_mean" => {
                            if a.len() != 1 {
                                return Err("vec_mean expects list".into());
                            }
                            let xs = as_num_list(&a[0])?;
                            if xs.is_empty() {
                                return Err("vec_mean requires non-empty list".into());
                            }
                            Val::Num(xs.iter().sum::<f64>() / xs.len() as f64)
                        }
                        "vec_variance" => {
                            if a.len() != 1 {
                                return Err("vec_variance expects list".into());
                            }
                            let xs = as_num_list(&a[0])?;
                            if xs.is_empty() {
                                return Err("vec_variance requires non-empty list".into());
                            }
                            let m = xs.iter().sum::<f64>() / xs.len() as f64;
                            Val::Num(
                                xs.iter().map(|x| (x - m) * (x - m)).sum::<f64>() / xs.len() as f64,
                            )
                        }
                        "vec_std" => {
                            if a.len() != 1 {
                                return Err("vec_std expects list".into());
                            }
                            let xs = as_num_list(&a[0])?;
                            if xs.is_empty() {
                                return Err("vec_std requires non-empty list".into());
                            }
                            let m = xs.iter().sum::<f64>() / xs.len() as f64;
                            let v =
                                xs.iter().map(|x| (x - m) * (x - m)).sum::<f64>() / xs.len() as f64;
                            Val::Num(v.sqrt())
                        }
                        "vec_median" => {
                            if a.len() != 1 {
                                return Err("vec_median expects list".into());
                            }
                            let mut xs = as_num_list(&a[0])?;
                            if xs.is_empty() {
                                return Err("vec_median requires non-empty list".into());
                            }
                            xs.sort_by(|x, y| {
                                x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal)
                            });
                            let n = xs.len();
                            Val::Num(if n % 2 == 1 {
                                xs[n / 2]
                            } else {
                                (xs[n / 2 - 1] + xs[n / 2]) / 2.0
                            })
                        }
                        "vec_percentile" => {
                            if a.len() != 2 {
                                return Err("vec_percentile expects list,p".into());
                            }
                            let mut xs = as_num_list(&a[0])?;
                            let p = match a[1] {
                                Val::Num(n) => n.clamp(0.0, 100.0),
                                _ => return Err("p must be Number".into()),
                            };
                            if xs.is_empty() {
                                return Err("vec_percentile requires non-empty list".into());
                            }
                            xs.sort_by(|x, y| {
                                x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal)
                            });
                            let pos = (p / 100.0) * (xs.len() - 1) as f64;
                            let lo = pos.floor() as usize;
                            let hi = pos.ceil() as usize;
                            let value = if lo == hi {
                                xs[lo]
                            } else {
                                xs[lo] + (xs[hi] - xs[lo]) * (pos - lo as f64)
                            };
                            Val::Num(value)
                        }
                        "covariance" => {
                            if a.len() != 2 {
                                return Err("covariance expects two lists".into());
                            }
                            let x = as_num_list(&a[0])?;
                            let y = as_num_list(&a[1])?;
                            if x.len() != y.len() || x.is_empty() {
                                return Err("covariance length mismatch".into());
                            }
                            let mx = x.iter().sum::<f64>() / x.len() as f64;
                            let my = y.iter().sum::<f64>() / y.len() as f64;
                            Val::Num(
                                x.iter()
                                    .zip(y.iter())
                                    .map(|(a, b)| (a - mx) * (b - my))
                                    .sum::<f64>()
                                    / x.len() as f64,
                            )
                        }
                        "correlation" => {
                            if a.len() != 2 {
                                return Err("correlation expects two lists".into());
                            }
                            let x = as_num_list(&a[0])?;
                            let y = as_num_list(&a[1])?;
                            if x.len() != y.len() || x.is_empty() {
                                return Err("correlation length mismatch".into());
                            }
                            let mx = x.iter().sum::<f64>() / x.len() as f64;
                            let my = y.iter().sum::<f64>() / y.len() as f64;
                            let cov = x
                                .iter()
                                .zip(y.iter())
                                .map(|(a, b)| (a - mx) * (b - my))
                                .sum::<f64>();
                            let sx = x.iter().map(|a| (a - mx) * (a - mx)).sum::<f64>().sqrt();
                            let sy = y.iter().map(|b| (b - my) * (b - my)).sum::<f64>().sqrt();
                            Val::Num(if sx == 0.0 || sy == 0.0 {
                                0.0
                            } else {
                                cov / (sx * sy)
                            })
                        }
                        "vec_dot" => {
                            if a.len() != 2 {
                                return Err("vec_dot expects two lists".into());
                            }
                            let x = as_num_list(&a[0])?;
                            let y = as_num_list(&a[1])?;
                            if x.len() != y.len() {
                                return Err("vec_dot length mismatch".into());
                            }
                            Val::Num(x.iter().zip(y.iter()).map(|(a, b)| a * b).sum())
                        }
                        "vec_add" => {
                            if a.len() != 2 {
                                return Err("vec_add expects two lists".into());
                            }
                            let x = as_num_list(&a[0])?;
                            let y = as_num_list(&a[1])?;
                            if x.len() != y.len() {
                                return Err("vec_add length mismatch".into());
                            }
                            nums_to_val(x.iter().zip(y.iter()).map(|(a, b)| a + b).collect())
                        }
                        "vec_scale" => {
                            if a.len() != 2 {
                                return Err("vec_scale expects list,scalar".into());
                            }
                            let x = as_num_list(&a[0])?;
                            let s = match a[1] {
                                Val::Num(n) => n,
                                _ => return Err("scalar must be Number".into()),
                            };
                            nums_to_val(x.into_iter().map(|v| v * s).collect())
                        }
                        "transpose" => {
                            if a.len() != 1 {
                                return Err("transpose expects matrix".into());
                            }
                            let m = as_matrix(&a[0])?;
                            if m.is_empty() {
                                Val::List(vec![])
                            } else {
                                let cols = m[0].len();
                                if m.iter().any(|r| r.len() != cols) {
                                    return Err("ragged matrix".into());
                                }
                                let mut out = vec![vec![0.0; m.len()]; cols];
                                for i in 0..m.len() {
                                    for j in 0..cols {
                                        out[j][i] = m[i][j];
                                    }
                                }
                                matrix_to_val(out)
                            }
                        }
                        "matmul" => {
                            if a.len() != 2 {
                                return Err("matmul expects two matrices".into());
                            }
                            let x = as_matrix(&a[0])?;
                            let y = as_matrix(&a[1])?;
                            if x.is_empty() || y.is_empty() {
                                return Err("matmul requires non-empty matrices".into());
                            }
                            let n = x[0].len();
                            if x.iter().any(|r| r.len() != n)
                                || y.iter().any(|r| r.len() != y[0].len())
                                || y.len() != n
                            {
                                return Err("matmul dimension mismatch".into());
                            }
                            let p = y[0].len();
                            let mut out = vec![vec![0.0; p]; x.len()];
                            for i in 0..x.len() {
                                for j in 0..p {
                                    for k in 0..n {
                                        out[i][j] += x[i][k] * y[k][j];
                                    }
                                }
                            }
                            matrix_to_val(out)
                        }
                        "matrix_det" => {
                            if a.len() != 1 {
                                return Err("matrix_det expects matrix".into());
                            }
                            Val::Num(matrix_determinant(as_matrix(&a[0])?)?)
                        }
                        "matrix_inverse" => {
                            if a.len() != 1 {
                                return Err("matrix_inverse expects matrix".into());
                            }
                            matrix_to_val(matrix_inverse(as_matrix(&a[0])?)?)
                        }
                        "linear_solve" => {
                            if a.len() != 2 {
                                return Err("linear_solve expects matrix,vector".into());
                            }
                            nums_to_val(solve_linear_system(
                                as_matrix(&a[0])?,
                                as_num_list(&a[1])?,
                            )?)
                        }
                        "linreg_fit" => {
                            if a.len() != 2 {
                                return Err("linreg_fit expects x,y".into());
                            }
                            let x = as_num_list(&a[0])?;
                            let y = as_num_list(&a[1])?;
                            if x.len() != y.len() || x.len() < 2 {
                                return Err("linreg_fit requires equal lists length >=2".into());
                            }
                            let mx = x.iter().sum::<f64>() / x.len() as f64;
                            let my = y.iter().sum::<f64>() / y.len() as f64;
                            let num = x
                                .iter()
                                .zip(y.iter())
                                .map(|(a, b)| (a - mx) * (b - my))
                                .sum::<f64>();
                            let den = x.iter().map(|a| (a - mx) * (a - mx)).sum::<f64>();
                            if den == 0.0 {
                                return Err("linreg_fit zero x variance".into());
                            }
                            let slope = num / den;
                            let intercept = my - slope * mx;
                            let ss_tot = y.iter().map(|v| (v - my) * (v - my)).sum::<f64>();
                            let ss_res = x
                                .iter()
                                .zip(y.iter())
                                .map(|(xx, yy)| {
                                    let e = yy - (intercept + slope * xx);
                                    e * e
                                })
                                .sum::<f64>();
                            let r2 = if ss_tot == 0.0 {
                                1.0
                            } else {
                                1.0 - ss_res / ss_tot
                            };
                            Val::Map(vec![
                                (Val::Str("slope".into()), Val::Num(slope)),
                                (Val::Str("intercept".into()), Val::Num(intercept)),
                                (Val::Str("r2".into()), Val::Num(r2)),
                            ])
                        }
                        "linreg_predict" => {
                            if a.len() != 2 {
                                return Err("linreg_predict expects model,x".into());
                            }
                            let pairs = match &a[0] {
                                Val::Map(p) => p,
                                _ => return Err("model must be Map".into()),
                            };
                            let getn = |name: &str| -> Result<f64, String> {
                                pairs
                                    .iter()
                                    .find(|(k, _)| *k == Val::Str(name.into()))
                                    .and_then(
                                        |(_, v)| if let Val::Num(n) = v { Some(*n) } else { None },
                                    )
                                    .ok_or(format!("model missing {name}"))
                            };
                            let x = match a[1] {
                                Val::Num(n) => n,
                                _ => return Err("x must be Number".into()),
                            };
                            Val::Num(getn("intercept")? + getn("slope")? * x)
                        }
                        "knn_predict" => {
                            if a.len() != 4 {
                                return Err("knn_predict expects train_x,train_y,point,k".into());
                            }
                            let xs = as_matrix(&a[0])?;
                            let ys = match &a[1] {
                                Val::List(v) => v.clone(),
                                _ => return Err("train_y must be List".into()),
                            };
                            let point = as_num_list(&a[2])?;
                            let k = match a[3] {
                                Val::Num(n) => n as usize,
                                _ => return Err("k must be Number".into()),
                            };
                            if xs.len() != ys.len() || xs.is_empty() || k == 0 {
                                return Err("invalid knn data".into());
                            }
                            let mut pairs: Vec<(f64, Val)> = xs
                                .iter()
                                .zip(ys.iter())
                                .map(|(x, y)| (distance_sq(x, &point), y.clone()))
                                .collect();
                            pairs.sort_by(|a, b| {
                                a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal)
                            });
                            let mut counts: Vec<(Val, usize)> = vec![];
                            for (_, label) in pairs.into_iter().take(k.min(xs.len())) {
                                if let Some((_, c)) = counts.iter_mut().find(|(l, _)| *l == label) {
                                    *c += 1
                                } else {
                                    counts.push((label, 1))
                                }
                            }
                            counts
                                .into_iter()
                                .max_by_key(|(_, c)| *c)
                                .map(|(l, _)| l)
                                .unwrap_or(Val::Null)
                        }
                        "sigmoid" => {
                            if a.len() != 1 {
                                return Err("sigmoid expects number".into());
                            }
                            let x = match a[0] {
                                Val::Num(n) => n,
                                _ => return Err("sigmoid expects Number".into()),
                            };
                            Val::Num(1.0 / (1.0 + (-x).exp()))
                        }
                        "tensor" => {
                            if a.is_empty() || a.len() > 2 {
                                return Err("tensor expects values[,requires_grad]".into());
                            }
                            let requires_grad = if a.len() == 2 {
                                match a[1] {
                                    Val::Bool(b) => b,
                                    _ => return Err("requires_grad must be Bool".into()),
                                }
                            } else {
                                false
                            };
                            tensor_from_value(&a[0], requires_grad)?
                        }
                        "tensor_values" => {
                            if a.len() != 1 {
                                return Err("tensor_values expects tensor".into());
                            }
                            let id = tensor_id(&a[0])?;
                            nums_to_val(tensor_snapshot(id)?.data)
                        }
                        "tensor_shape" => {
                            if a.len() != 1 {
                                return Err("tensor_shape expects tensor".into());
                            }
                            let id = tensor_id(&a[0])?;
                            Val::List(
                                tensor_snapshot(id)?
                                    .shape
                                    .into_iter()
                                    .map(|n| Val::Num(n as f64))
                                    .collect(),
                            )
                        }
                        "tensor_grad" => {
                            if a.len() != 1 {
                                return Err("tensor_grad expects tensor".into());
                            }
                            let id = tensor_id(&a[0])?;
                            nums_to_val(tensor_snapshot(id)?.grad)
                        }
                        "tensor_add" | "tensor_sub" | "tensor_mul" | "tensor_div" => {
                            if a.len() != 2 {
                                return Err(format!("{n} expects two tensors"));
                            }
                            let left_id = tensor_id(&a[0])?;
                            let right_id = tensor_id(&a[1])?;
                            let left = tensor_snapshot(left_id)?;
                            let right = tensor_snapshot(right_id)?;
                            if left.shape != right.shape || left.data.len() != right.data.len() {
                                return Err("tensor shapes must match".into());
                            }
                            if n == "tensor_div" && right.data.contains(&0.0) {
                                return Err("tensor_div division by zero".into());
                            }
                            let data = match n.as_str() {
                                "tensor_add" => left
                                    .data
                                    .iter()
                                    .zip(&right.data)
                                    .map(|(x, y)| x + y)
                                    .collect(),
                                "tensor_sub" => left
                                    .data
                                    .iter()
                                    .zip(&right.data)
                                    .map(|(x, y)| x - y)
                                    .collect(),
                                "tensor_mul" => left
                                    .data
                                    .iter()
                                    .zip(&right.data)
                                    .map(|(x, y)| x * y)
                                    .collect(),
                                _ => left
                                    .data
                                    .iter()
                                    .zip(&right.data)
                                    .map(|(x, y)| x / y)
                                    .collect(),
                            };
                            let requires_grad = left.requires_grad || right.requires_grad;
                            let op = match n.as_str() {
                                "tensor_add" => TensorOp::Add(left_id, right_id),
                                "tensor_sub" => TensorOp::Sub(left_id, right_id),
                                "tensor_mul" => TensorOp::Mul(left_id, right_id),
                                _ => TensorOp::Div(left_id, right_id),
                            };
                            tensor_store(data, left.shape, requires_grad, op)?
                        }
                        "tensor_neg" => {
                            if a.len() != 1 {
                                return Err("tensor_neg expects tensor".into());
                            }
                            let id = tensor_id(&a[0])?;
                            let t = tensor_snapshot(id)?;
                            let data = t.data.iter().map(|x| -*x).collect();
                            tensor_store(data, t.shape, t.requires_grad, TensorOp::Neg(id))?
                        }
                        "tensor_pow" => {
                            if a.len() != 2 {
                                return Err("tensor_pow expects tensor,exponent".into());
                            }
                            let id = tensor_id(&a[0])?;
                            let exponent = match a[1] {
                                Val::Num(n) => n,
                                _ => return Err("tensor_pow exponent must be Number".into()),
                            };
                            let t = tensor_snapshot(id)?;
                            let data = t.data.iter().map(|x| x.powf(exponent)).collect();
                            tensor_store(
                                data,
                                t.shape,
                                t.requires_grad,
                                TensorOp::Pow(id, exponent),
                            )?
                        }
                        "tensor_softmax" => {
                            if a.len() != 1 {
                                return Err("tensor_softmax expects tensor".into());
                            }
                            let id = tensor_id(&a[0])?;
                            let t = tensor_snapshot(id)?;
                            let (rows, cols) = match t.shape.as_slice() {
                                [n] => (1usize, *n),
                                [r, c] => (*r, *c),
                                _ => return Err("tensor_softmax expects 1D or 2D tensor".into()),
                            };
                            let mut out = vec![0.0; t.data.len()];
                            for row in 0..rows {
                                let start = row * cols;
                                let end = start + cols;
                                let max = t.data[start..end]
                                    .iter()
                                    .copied()
                                    .fold(f64::NEG_INFINITY, f64::max);
                                let mut denom = 0.0;
                                for c in 0..cols {
                                    let e = (t.data[start + c] - max).exp();
                                    out[start + c] = e;
                                    denom += e;
                                }
                                for c in 0..cols {
                                    out[start + c] /= denom;
                                }
                            }
                            tensor_store(
                                out,
                                t.shape,
                                t.requires_grad,
                                TensorOp::SoftmaxRows(id, rows, cols),
                            )?
                        }
                        "tensor_detach" => {
                            if a.len() != 1 {
                                return Err("tensor_detach expects tensor".into());
                            }
                            let id = tensor_id(&a[0])?;
                            let t = tensor_snapshot(id)?;
                            tensor_store(t.data, t.shape, false, TensorOp::Leaf)?
                        }
                        "tensor_numel" => {
                            if a.len() != 1 {
                                return Err("tensor_numel expects tensor".into());
                            }
                            let id = tensor_id(&a[0])?;
                            Val::Num(tensor_snapshot(id)?.data.len() as f64)
                        }
                        "tensor_relu" => {
                            if a.len() != 1 {
                                return Err("tensor_relu expects tensor".into());
                            }
                            let id = tensor_id(&a[0])?;
                            let t = tensor_snapshot(id)?;
                            let data = t.data.iter().map(|x| x.max(0.0)).collect();
                            tensor_store(data, t.shape, t.requires_grad, TensorOp::Relu(id))?
                        }
                        "tensor_sigmoid" | "tensor_tanh" | "tensor_exp" | "tensor_log" => {
                            if a.len() != 1 {
                                return Err(format!("{n} expects tensor"));
                            }
                            let id = tensor_id(&a[0])?;
                            let t = tensor_snapshot(id)?;
                            let data: Vec<f64> = match n.as_str() {
                                "tensor_sigmoid" => {
                                    t.data.iter().map(|x| 1.0 / (1.0 + (-x).exp())).collect()
                                }
                                "tensor_tanh" => t.data.iter().map(|x| x.tanh()).collect(),
                                "tensor_exp" => t.data.iter().map(|x| x.exp()).collect(),
                                _ => {
                                    if t.data.iter().any(|x| *x <= 0.0) {
                                        return Err(
                                            "tensor_log requires strictly positive values".into()
                                        );
                                    }
                                    t.data.iter().map(|x| x.ln()).collect()
                                }
                            };
                            let op = match n.as_str() {
                                "tensor_sigmoid" => TensorOp::Sigmoid(id),
                                "tensor_tanh" => TensorOp::Tanh(id),
                                "tensor_exp" => TensorOp::Exp(id),
                                _ => TensorOp::Log(id),
                            };
                            tensor_store(data, t.shape, t.requires_grad, op)?
                        }
                        "tensor_reshape" => {
                            if a.len() != 2 {
                                return Err("tensor_reshape expects tensor,shape".into());
                            }
                            let id = tensor_id(&a[0])?;
                            let t = tensor_snapshot(id)?;
                            let dims = as_num_list(&a[1])?;
                            let mut shape = Vec::with_capacity(dims.len());
                            for d in dims {
                                if d < 0.0 || d.fract() != 0.0 {
                                    return Err(
                                        "tensor_reshape dimensions must be non-negative integers"
                                            .into(),
                                    );
                                }
                                shape.push(d as usize);
                            }
                            let size = if shape.is_empty() {
                                1
                            } else {
                                shape.iter().product()
                            };
                            if size != t.data.len() {
                                return Err("tensor_reshape element count mismatch".into());
                            }
                            tensor_store(t.data, shape, t.requires_grad, TensorOp::Reshape(id))?
                        }
                        "tensor_transpose" => {
                            if a.len() != 1 {
                                return Err("tensor_transpose expects tensor".into());
                            }
                            let id = tensor_id(&a[0])?;
                            let t = tensor_snapshot(id)?;
                            if t.shape.len() != 2 {
                                return Err("tensor_transpose expects a 2D tensor".into());
                            }
                            let (rows, cols) = (t.shape[0], t.shape[1]);
                            let mut out = vec![0.0; t.data.len()];
                            for r in 0..rows {
                                for c in 0..cols {
                                    out[c * rows + r] = t.data[r * cols + c];
                                }
                            }
                            tensor_store(
                                out,
                                vec![cols, rows],
                                t.requires_grad,
                                TensorOp::Transpose(id, rows, cols),
                            )?
                        }
                        "tensor_save" => {
                            if a.len() != 2 {
                                return Err("tensor_save expects tensor,path".into());
                            }
                            let id = tensor_id(&a[0])?;
                            let path = match &a[1] {
                                Val::Str(s) => s,
                                _ => return Err("tensor_save path must be String".into()),
                            };
                            let t = tensor_snapshot(id)?;
                            let body = serde_json::json!({
                                "format": "veyra.tensor.v1",
                                "shape": t.shape,
                                "data": t.data,
                                "requires_grad": t.requires_grad
                            });
                            fs::write(
                                path,
                                serde_json::to_vec_pretty(&body).map_err(|e| e.to_string())?,
                            )
                            .map_err(|e| e.to_string())?;
                            Val::Null
                        }
                        "tensor_load" => {
                            if a.len() != 1 {
                                return Err("tensor_load expects path".into());
                            }
                            let path = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("tensor_load path must be String".into()),
                            };
                            let body: JsonValue =
                                serde_json::from_slice(&fs::read(path).map_err(|e| e.to_string())?)
                                    .map_err(|e| e.to_string())?;
                            if body.get("format").and_then(JsonValue::as_str)
                                != Some("veyra.tensor.v1")
                            {
                                return Err("unsupported tensor file format".into());
                            }
                            let shape = body
                                .get("shape")
                                .and_then(JsonValue::as_array)
                                .ok_or("tensor file missing shape")?
                                .iter()
                                .map(|v| {
                                    v.as_u64().map(|n| n as usize).ok_or("invalid tensor shape")
                                })
                                .collect::<Result<Vec<_>, _>>()?;
                            let data = body
                                .get("data")
                                .and_then(JsonValue::as_array)
                                .ok_or("tensor file missing data")?
                                .iter()
                                .map(|v| v.as_f64().ok_or("invalid tensor value"))
                                .collect::<Result<Vec<_>, _>>()?;
                            let requires_grad = body
                                .get("requires_grad")
                                .and_then(JsonValue::as_bool)
                                .unwrap_or(false);
                            let size = if shape.is_empty() {
                                1
                            } else {
                                shape.iter().product()
                            };
                            if size != data.len() {
                                return Err("tensor file shape/data mismatch".into());
                            }
                            tensor_store(data, shape, requires_grad, TensorOp::Leaf)?
                        }
                        "tensor_sum" | "tensor_mean" => {
                            if a.len() != 1 {
                                return Err(format!("{n} expects tensor"));
                            }
                            let id = tensor_id(&a[0])?;
                            let t = tensor_snapshot(id)?;
                            let sum: f64 = t.data.iter().sum();
                            let (value, op) = if n == "tensor_mean" {
                                (
                                    if t.data.is_empty() {
                                        0.0
                                    } else {
                                        sum / t.data.len() as f64
                                    },
                                    TensorOp::Mean(id),
                                )
                            } else {
                                (sum, TensorOp::Sum(id))
                            };
                            tensor_store(vec![value], vec![], t.requires_grad, op)?
                        }
                        "tensor_matmul" => {
                            if a.len() != 2 {
                                return Err("tensor_matmul expects two tensors".into());
                            }
                            let left_id = tensor_id(&a[0])?;
                            let right_id = tensor_id(&a[1])?;
                            let left = tensor_snapshot(left_id)?;
                            let right = tensor_snapshot(right_id)?;
                            if left.shape.len() != 2 || right.shape.len() != 2 {
                                return Err("tensor_matmul expects 2D tensors".into());
                            }
                            let (m, k) = (left.shape[0], left.shape[1]);
                            let (k2, ncols) = (right.shape[0], right.shape[1]);
                            if k != k2 {
                                return Err("tensor_matmul shape mismatch".into());
                            }
                            let mut out = vec![0.0; m * ncols];
                            for i in 0..m {
                                for j in 0..ncols {
                                    let mut s = 0.0;
                                    for p in 0..k {
                                        s += left.data[i * k + p] * right.data[p * ncols + j];
                                    }
                                    out[i * ncols + j] = s;
                                }
                            }
                            tensor_store(
                                out,
                                vec![m, ncols],
                                left.requires_grad || right.requires_grad,
                                TensorOp::MatMul(left_id, right_id, m, k, ncols),
                            )?
                        }
                        "tensor_backward" => {
                            if a.len() != 1 {
                                return Err("tensor_backward expects scalar tensor".into());
                            }
                            let id = tensor_id(&a[0])?;
                            tensor_backward(id)?;
                            Val::Null
                        }
                        "tensor_step" => {
                            if a.len() != 2 {
                                return Err("tensor_step expects tensor,learning_rate".into());
                            }
                            let id = tensor_id(&a[0])?;
                            let lr = match a[1] {
                                Val::Num(n) => n,
                                _ => return Err("learning_rate must be Number".into()),
                            };
                            let mut store =
                                tensors().lock().map_err(|_| "tensor registry poisoned")?;
                            let t = store.get_mut(&id).ok_or("unknown tensor")?;
                            if !t.requires_grad {
                                return Err("tensor_step requires trainable tensor".into());
                            }
                            for i in 0..t.data.len() {
                                t.data[i] -= lr * t.grad[i];
                            }
                            Val::Null
                        }
                        "tensor_adam_step" => {
                            if a.len() != 5 {
                                return Err(
                                    "tensor_adam_step expects tensor,lr,beta1,beta2,epsilon".into(),
                                );
                            }
                            let id = tensor_id(&a[0])?;
                            let number = |v: &Val, name: &str| match v {
                                Val::Num(n) => Ok(*n),
                                _ => Err(format!("{name} must be Number")),
                            };
                            let lr = number(&a[1], "learning_rate")?;
                            let beta1 = number(&a[2], "beta1")?;
                            let beta2 = number(&a[3], "beta2")?;
                            let epsilon = number(&a[4], "epsilon")?;
                            if !(0.0..1.0).contains(&beta1)
                                || !(0.0..1.0).contains(&beta2)
                                || lr <= 0.0
                                || epsilon <= 0.0
                            {
                                return Err("invalid Adam hyperparameters".into());
                            }
                            let tensor = tensor_snapshot(id)?;
                            if !tensor.requires_grad {
                                return Err("tensor_adam_step requires trainable tensor".into());
                            }
                            let updates = {
                                let mut states =
                                    adam_states().lock().map_err(|_| "adam state poisoned")?;
                                let state = states.entry(id).or_insert_with(|| AdamState {
                                    m: vec![0.0; tensor.data.len()],
                                    v: vec![0.0; tensor.data.len()],
                                    step: 0,
                                });
                                if state.m.len() != tensor.data.len() {
                                    *state = AdamState {
                                        m: vec![0.0; tensor.data.len()],
                                        v: vec![0.0; tensor.data.len()],
                                        step: 0,
                                    };
                                }
                                state.step += 1;
                                let bias1 = 1.0 - beta1.powf(state.step as f64);
                                let bias2 = 1.0 - beta2.powf(state.step as f64);
                                let mut updates = vec![0.0; tensor.data.len()];
                                for i in 0..tensor.data.len() {
                                    let g = tensor.grad[i];
                                    state.m[i] = beta1 * state.m[i] + (1.0 - beta1) * g;
                                    state.v[i] = beta2 * state.v[i] + (1.0 - beta2) * g * g;
                                    let m_hat = state.m[i] / bias1;
                                    let v_hat = state.v[i] / bias2;
                                    updates[i] = lr * m_hat / (v_hat.sqrt() + epsilon);
                                }
                                updates
                            };
                            let mut store =
                                tensors().lock().map_err(|_| "tensor registry poisoned")?;
                            let t = store.get_mut(&id).ok_or("unknown tensor")?;
                            for (value, update) in t.data.iter_mut().zip(updates) {
                                *value -= update;
                            }
                            Val::Null
                        }
                        "tensor_reset_optimizer" => {
                            if a.len() != 1 {
                                return Err("tensor_reset_optimizer expects tensor".into());
                            }
                            let id = tensor_id(&a[0])?;
                            adam_states()
                                .lock()
                                .map_err(|_| "adam state poisoned")?
                                .remove(&id);
                            Val::Null
                        }
                        "tensor_zero_grad" => {
                            if a.len() != 1 {
                                return Err("tensor_zero_grad expects tensor".into());
                            }
                            let id = tensor_id(&a[0])?;
                            let mut store =
                                tensors().lock().map_err(|_| "tensor registry poisoned")?;
                            let t = store.get_mut(&id).ok_or("unknown tensor")?;
                            for g in &mut t.grad {
                                *g = 0.0;
                            }
                            Val::Null
                        }
                        "accuracy_score" => {
                            if a.len() != 2 {
                                return Err("accuracy_score expects actual,predicted".into());
                            }
                            let actual = match &a[0] {
                                Val::List(v) => v,
                                _ => return Err("actual must be List".into()),
                            };
                            let predicted = match &a[1] {
                                Val::List(v) => v,
                                _ => return Err("predicted must be List".into()),
                            };
                            if actual.len() != predicted.len() || actual.is_empty() {
                                return Err("accuracy_score length mismatch".into());
                            }
                            let correct = actual
                                .iter()
                                .zip(predicted.iter())
                                .filter(|(x, y)| x == y)
                                .count();
                            Val::Num(correct as f64 / actual.len() as f64)
                        }
                        "classification_metrics" => {
                            if a.len() != 2 {
                                return Err(
                                    "classification_metrics expects actual,predicted".into()
                                );
                            }
                            let actual = match &a[0] {
                                Val::List(v) => v,
                                _ => return Err("actual must be List".into()),
                            };
                            let predicted = match &a[1] {
                                Val::List(v) => v,
                                _ => return Err("predicted must be List".into()),
                            };
                            if actual.len() != predicted.len() || actual.is_empty() {
                                return Err("classification_metrics length mismatch".into());
                            }
                            let positive = |v: &Val| {
                                matches!(v,Val::Num(n) if *n==1.0) || matches!(v, Val::Bool(true))
                            };
                            let mut tp = 0.0;
                            let mut tn = 0.0;
                            let mut fp = 0.0;
                            let mut fnn = 0.0;
                            for (x, y) in actual.iter().zip(predicted.iter()) {
                                match (positive(x), positive(y)) {
                                    (true, true) => tp += 1.0,
                                    (false, false) => tn += 1.0,
                                    (false, true) => fp += 1.0,
                                    (true, false) => fnn += 1.0,
                                }
                            }
                            let precision = if tp + fp == 0.0 { 0.0 } else { tp / (tp + fp) };
                            let recall = if tp + fnn == 0.0 {
                                0.0
                            } else {
                                tp / (tp + fnn)
                            };
                            let f1 = if precision + recall == 0.0 {
                                0.0
                            } else {
                                2.0 * precision * recall / (precision + recall)
                            };
                            let accuracy = (tp + tn) / (tp + tn + fp + fnn);
                            Val::Map(vec![
                                (Val::Str("accuracy".into()), Val::Num(accuracy)),
                                (Val::Str("precision".into()), Val::Num(precision)),
                                (Val::Str("recall".into()), Val::Num(recall)),
                                (Val::Str("f1".into()), Val::Num(f1)),
                                (Val::Str("tp".into()), Val::Num(tp)),
                                (Val::Str("tn".into()), Val::Num(tn)),
                                (Val::Str("fp".into()), Val::Num(fp)),
                                (Val::Str("fn".into()), Val::Num(fnn)),
                            ])
                        }
                        "train_test_split" => {
                            if a.len() != 3 {
                                return Err("train_test_split expects x,y,test_ratio".into());
                            }
                            let x = match &a[0] {
                                Val::List(v) => v,
                                _ => return Err("x must be List".into()),
                            };
                            let y = match &a[1] {
                                Val::List(v) => v,
                                _ => return Err("y must be List".into()),
                            };
                            let ratio = match a[2] {
                                Val::Num(n) => n,
                                _ => return Err("test_ratio must be Number".into()),
                            };
                            if x.len() != y.len() || x.is_empty() {
                                return Err("train_test_split length mismatch".into());
                            }
                            if !(0.0..1.0).contains(&ratio) {
                                return Err("test_ratio must be between 0 and 1".into());
                            }
                            let test_count = ((x.len() as f64) * ratio).round().max(1.0) as usize;
                            let split = x.len().saturating_sub(test_count.min(x.len()));
                            Val::Map(vec![
                                (Val::Str("train_x".into()), Val::List(x[..split].to_vec())),
                                (Val::Str("train_y".into()), Val::List(y[..split].to_vec())),
                                (Val::Str("test_x".into()), Val::List(x[split..].to_vec())),
                                (Val::Str("test_y".into()), Val::List(y[split..].to_vec())),
                            ])
                        }
                        "mse" => {
                            if a.len() != 2 {
                                return Err("mse expects actual,predicted".into());
                            }
                            let x = as_num_list(&a[0])?;
                            let y = as_num_list(&a[1])?;
                            if x.len() != y.len() || x.is_empty() {
                                return Err("mse length mismatch".into());
                            }
                            Val::Num(
                                x.iter()
                                    .zip(y.iter())
                                    .map(|(a, b)| (a - b) * (a - b))
                                    .sum::<f64>()
                                    / x.len() as f64,
                            )
                        }
                        "normalize" => {
                            if a.len() != 1 {
                                return Err("normalize expects list".into());
                            }
                            let xs = as_num_list(&a[0])?;
                            if xs.is_empty() {
                                return Err("normalize requires non-empty list".into());
                            }
                            let min = xs.iter().fold(f64::INFINITY, |m, x| m.min(*x));
                            let max = xs.iter().fold(f64::NEG_INFINITY, |m, x| m.max(*x));
                            let span = max - min;
                            if span == 0.0 {
                                nums_to_val(vec![0.0; xs.len()])
                            } else {
                                nums_to_val(xs.into_iter().map(|x| (x - min) / span).collect())
                            }
                        }
                        "relu" => {
                            if a.len() != 1 {
                                return Err("relu expects list".into());
                            }
                            nums_to_val(
                                as_num_list(&a[0])?
                                    .into_iter()
                                    .map(|x| x.max(0.0))
                                    .collect(),
                            )
                        }
                        "softmax" => {
                            if a.len() != 1 {
                                return Err("softmax expects list".into());
                            }
                            let xs = as_num_list(&a[0])?;
                            if xs.is_empty() {
                                return Err("softmax requires non-empty list".into());
                            }
                            let m = xs.iter().fold(f64::NEG_INFINITY, |m, x| m.max(*x));
                            let exps: Vec<f64> = xs.iter().map(|x| (x - m).exp()).collect();
                            let sum: f64 = exps.iter().sum();
                            nums_to_val(exps.into_iter().map(|x| x / sum).collect())
                        }
                        "dense" => {
                            if a.len() != 3 {
                                return Err("dense expects input,weights,bias".into());
                            }
                            let input = as_num_list(&a[0])?;
                            let weights = as_matrix(&a[1])?;
                            let bias = as_num_list(&a[2])?;
                            if weights.len() != bias.len()
                                || weights.iter().any(|r| r.len() != input.len())
                            {
                                return Err("dense dimension mismatch".into());
                            }
                            nums_to_val(
                                weights
                                    .iter()
                                    .zip(bias.iter())
                                    .map(|(row, b)| {
                                        row.iter()
                                            .zip(input.iter())
                                            .map(|(w, x)| w * x)
                                            .sum::<f64>()
                                            + b
                                    })
                                    .collect(),
                            )
                        }
                        "kmeans" => {
                            if a.len() != 3 {
                                return Err("kmeans expects data,k,iterations".into());
                            }
                            let data = as_matrix(&a[0])?;
                            let k = match a[1] {
                                Val::Num(n) => n as usize,
                                _ => return Err("k must be Number".into()),
                            };
                            let iters = match a[2] {
                                Val::Num(n) => n as usize,
                                _ => return Err("iterations must be Number".into()),
                            };
                            if data.is_empty() || k == 0 || k > data.len() {
                                return Err("invalid kmeans data".into());
                            }
                            let dims = data[0].len();
                            if dims == 0 || data.iter().any(|r| r.len() != dims) {
                                return Err("ragged kmeans data".into());
                            }
                            let mut centers = data[..k].to_vec();
                            for _ in 0..iters {
                                let mut sums = vec![vec![0.0; dims]; k];
                                let mut counts = vec![0usize; k];
                                for point in &data {
                                    let mut best = 0usize;
                                    let mut bestd = distance_sq(point, &centers[0]);
                                    for i in 1..k {
                                        let dist = distance_sq(point, &centers[i]);
                                        if dist < bestd {
                                            best = i;
                                            bestd = dist;
                                        }
                                    }
                                    counts[best] += 1;
                                    for j in 0..dims {
                                        sums[best][j] += point[j];
                                    }
                                }
                                for i in 0..k {
                                    if counts[i] > 0 {
                                        for j in 0..dims {
                                            centers[i][j] = sums[i][j] / counts[i] as f64;
                                        }
                                    }
                                }
                            }
                            matrix_to_val(centers)
                        }
                        "table_column" => {
                            if a.len() != 2 {
                                return Err("table_column expects rows,name".into());
                            }
                            let rows = match &a[0] {
                                Val::List(v) => v,
                                _ => return Err("rows must be List".into()),
                            };
                            let name = match &a[1] {
                                Val::Str(s) => Val::Str(s.clone()),
                                _ => return Err("name must be String".into()),
                            };
                            let mut out = vec![];
                            for row in rows {
                                let pairs = match row {
                                    Val::Map(p) => p,
                                    _ => return Err("rows must contain Map values".into()),
                                };
                                out.push(map_lookup(pairs, &name).cloned().unwrap_or(Val::Null));
                            }
                            Val::List(out)
                        }
                        "table_filter_eq" => {
                            if a.len() != 3 {
                                return Err("table_filter_eq expects rows,name,value".into());
                            }
                            let rows = match &a[0] {
                                Val::List(v) => v,
                                _ => return Err("rows must be List".into()),
                            };
                            let key = match &a[1] {
                                Val::Str(s) => Val::Str(s.clone()),
                                _ => return Err("name must be String".into()),
                            };
                            let target = &a[2];
                            Val::List(
                                rows.iter()
                                    .filter(|row| match row {
                                        Val::Map(p) => map_lookup(p, &key)
                                            .map(|v| v == target)
                                            .unwrap_or(false),
                                        _ => false,
                                    })
                                    .cloned()
                                    .collect(),
                            )
                        }
                        "table_describe" => {
                            if a.len() != 2 {
                                return Err("table_describe expects rows,name".into());
                            }
                            let rows = match &a[0] {
                                Val::List(v) => v,
                                _ => return Err("rows must be List".into()),
                            };
                            let key = match &a[1] {
                                Val::Str(s) => Val::Str(s.clone()),
                                _ => return Err("name must be String".into()),
                            };
                            let nums: Vec<f64> = rows
                                .iter()
                                .filter_map(|row| match row {
                                    Val::Map(p) => map_lookup(p, &key).and_then(|v| match v {
                                        Val::Num(n) => Some(n.to_owned()),
                                        _ => None,
                                    }),
                                    _ => None,
                                })
                                .collect();
                            if nums.is_empty() {
                                return Err("table_describe found no numeric values".into());
                            }
                            let count = nums.len() as f64;
                            let mean = nums.iter().sum::<f64>() / count;
                            let min = nums.iter().fold(f64::INFINITY, |m, x| m.min(*x));
                            let max = nums.iter().fold(f64::NEG_INFINITY, |m, x| m.max(*x));
                            Val::Map(vec![
                                (Val::Str("count".into()), Val::Num(count)),
                                (Val::Str("mean".into()), Val::Num(mean)),
                                (Val::Str("min".into()), Val::Num(min)),
                                (Val::Str("max".into()), Val::Num(max)),
                            ])
                        }
                        "plot_svg" => {
                            if a.len() != 3 {
                                return Err("plot_svg expects path,xs,ys".into());
                            }
                            let path = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("path must be String".into()),
                            };
                            let xs = as_num_list(&a[1])?;
                            let ys = as_num_list(&a[2])?;
                            if xs.len() != ys.len() || xs.len() < 2 {
                                return Err("plot_svg requires equal lists length >=2".into());
                            }
                            let xmin = xs.iter().fold(f64::INFINITY, |m, x| m.min(*x));
                            let xmax = xs.iter().fold(f64::NEG_INFINITY, |m, x| m.max(*x));
                            let ymin = ys.iter().fold(f64::INFINITY, |m, x| m.min(*x));
                            let ymax = ys.iter().fold(f64::NEG_INFINITY, |m, x| m.max(*x));
                            let sx = if xmax == xmin { 1.0 } else { xmax - xmin };
                            let sy = if ymax == ymin { 1.0 } else { ymax - ymin };
                            let pts = xs
                                .iter()
                                .zip(ys.iter())
                                .map(|(x, y)| {
                                    let px = 50.0 + 700.0 * (x - xmin) / sx;
                                    let py = 450.0 - 400.0 * (y - ymin) / sy;
                                    format!("{px:.2},{py:.2}")
                                })
                                .collect::<Vec<_>>()
                                .join(" ");
                            let svg = format!(
                                "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"800\" height=\"500\"><rect width=\"100%\" height=\"100%\" fill=\"white\"/><line x1=\"50\" y1=\"450\" x2=\"750\" y2=\"450\" stroke=\"black\"/><line x1=\"50\" y1=\"50\" x2=\"50\" y2=\"450\" stroke=\"black\"/><polyline fill=\"none\" stroke=\"black\" stroke-width=\"2\" points=\"{}\"/></svg>",
                                svg_escape(&pts)
                            );
                            fs::write(path, svg).map_err(|e| e.to_string())?;
                            Val::Null
                        }
                        "scatter_svg" => {
                            if a.len() != 3 {
                                return Err("scatter_svg expects path,xs,ys".into());
                            }
                            let path = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("path must be String".into()),
                            };
                            let xs = as_num_list(&a[1])?;
                            let ys = as_num_list(&a[2])?;
                            if xs.len() != ys.len() || xs.is_empty() {
                                return Err("scatter_svg requires equal non-empty lists".into());
                            }
                            let xmin = xs.iter().fold(f64::INFINITY, |m, x| m.min(*x));
                            let xmax = xs.iter().fold(f64::NEG_INFINITY, |m, x| m.max(*x));
                            let ymin = ys.iter().fold(f64::INFINITY, |m, x| m.min(*x));
                            let ymax = ys.iter().fold(f64::NEG_INFINITY, |m, x| m.max(*x));
                            let sx = if xmax == xmin { 1.0 } else { xmax - xmin };
                            let sy = if ymax == ymin { 1.0 } else { ymax - ymin };
                            let dots = xs
                                .iter()
                                .zip(ys.iter())
                                .map(|(x, y)| {
                                    let px = 50.0 + 700.0 * (x - xmin) / sx;
                                    let py = 450.0 - 400.0 * (y - ymin) / sy;
                                    format!("<circle cx=\"{px:.2}\" cy=\"{py:.2}\" r=\"4\"/>")
                                })
                                .collect::<Vec<_>>()
                                .join("");
                            let svg = format!(
                                "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"800\" height=\"500\"><rect width=\"100%\" height=\"100%\" fill=\"white\"/><line x1=\"50\" y1=\"450\" x2=\"750\" y2=\"450\" stroke=\"black\"/><line x1=\"50\" y1=\"50\" x2=\"50\" y2=\"450\" stroke=\"black\"/>{dots}</svg>"
                            );
                            fs::write(path, svg).map_err(|e| e.to_string())?;
                            Val::Null
                        }
                        "bar_svg" => {
                            if a.len() != 3 {
                                return Err("bar_svg expects path,labels,values".into());
                            }
                            let path = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("path must be String".into()),
                            };
                            let labels = match &a[1] {
                                Val::List(v) => v,
                                _ => return Err("labels must be List".into()),
                            };
                            let vals = as_num_list(&a[2])?;
                            if labels.len() != vals.len() || vals.is_empty() {
                                return Err("bar_svg requires equal non-empty lists".into());
                            }
                            let max = vals.iter().copied().fold(0.0, f64::max).max(1.0);
                            let width = 700.0 / vals.len() as f64;
                            let mut body = String::new();
                            for (i, (label, value)) in labels.iter().zip(vals.iter()).enumerate() {
                                let h = 360.0 * (*value) / max;
                                let x = 55.0 + i as f64 * width;
                                let y = 440.0 - h;
                                body.push_str(&format!("<rect x=\"{x:.2}\" y=\"{y:.2}\" width=\"{:.2}\" height=\"{h:.2}\"/><text x=\"{:.2}\" y=\"470\" font-size=\"12\" text-anchor=\"middle\">{}</text>",(width-10.0).max(1.0),x+(width-10.0).max(1.0)/2.0,svg_escape(&label.show())));
                            }
                            let svg = format!(
                                "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"800\" height=\"500\"><rect width=\"100%\" height=\"100%\" fill=\"white\"/><line x1=\"50\" y1=\"440\" x2=\"760\" y2=\"440\" stroke=\"black\"/>{body}</svg>"
                            );
                            fs::write(path, svg).map_err(|e| e.to_string())?;
                            Val::Null
                        }
                        "histogram" => {
                            if a.len() != 2 {
                                return Err("histogram expects values,bins".into());
                            }
                            let vals = as_num_list(&a[0])?;
                            let bins = match a[1] {
                                Val::Num(n) => n.max(1.0) as usize,
                                _ => return Err("bins must be Number".into()),
                            };
                            if vals.is_empty() {
                                return Err("histogram requires values".into());
                            }
                            let min = vals.iter().copied().fold(f64::INFINITY, f64::min);
                            let max = vals.iter().copied().fold(f64::NEG_INFINITY, f64::max);
                            let span = (max - min).max(f64::EPSILON);
                            let mut counts = vec![0usize; bins];
                            for v in vals {
                                let mut idx = (((v - min) / span) * bins as f64).floor() as usize;
                                if idx >= bins {
                                    idx = bins - 1;
                                }
                                counts[idx] += 1;
                            }
                            Val::List(
                                counts
                                    .into_iter()
                                    .enumerate()
                                    .map(|(i, count)| {
                                        let lo = min + span * i as f64 / bins as f64;
                                        let hi = min + span * (i + 1) as f64 / bins as f64;
                                        Val::Map(vec![
                                            (Val::Str("low".into()), Val::Num(lo)),
                                            (Val::Str("high".into()), Val::Num(hi)),
                                            (Val::Str("count".into()), Val::Num(count as f64)),
                                        ])
                                    })
                                    .collect(),
                            )
                        }
                        "logreg_fit" => {
                            if a.len() != 4 {
                                return Err(
                                    "logreg_fit expects X,y,iterations,learning_rate".into()
                                );
                            }
                            let x = as_matrix(&a[0])?;
                            let y = as_num_list(&a[1])?;
                            let iterations = match a[2] {
                                Val::Num(n) => n.max(1.0) as usize,
                                _ => return Err("iterations must be Number".into()),
                            };
                            let lr = match a[3] {
                                Val::Num(n) => n,
                                _ => return Err("learning_rate must be Number".into()),
                            };
                            if x.is_empty() || x.len() != y.len() {
                                return Err("logreg_fit row mismatch".into());
                            }
                            let dims = x[0].len();
                            if dims == 0 || x.iter().any(|r| r.len() != dims) {
                                return Err("invalid feature matrix".into());
                            }
                            let mut w = vec![0.0; dims];
                            let mut b = 0.0;
                            for _ in 0..iterations {
                                let mut gw = vec![0.0; dims];
                                let mut gb = 0.0;
                                for (row, target) in x.iter().zip(y.iter()) {
                                    let z = b + w
                                        .iter()
                                        .zip(row.iter())
                                        .map(|(wi, xi)| wi * xi)
                                        .sum::<f64>();
                                    let p = 1.0 / (1.0 + (-z).exp());
                                    let e = p - target;
                                    for j in 0..dims {
                                        gw[j] += e * row[j];
                                    }
                                    gb += e;
                                }
                                let n = x.len() as f64;
                                for j in 0..dims {
                                    w[j] -= lr * gw[j] / n;
                                }
                                b -= lr * gb / n;
                            }
                            Val::Map(vec![
                                (Val::Str("weights".into()), nums_to_val(w)),
                                (Val::Str("bias".into()), Val::Num(b)),
                            ])
                        }
                        "logreg_predict" => {
                            if a.len() != 2 {
                                return Err("logreg_predict expects model,features".into());
                            }
                            let model = match &a[0] {
                                Val::Map(p) => p,
                                _ => return Err("model must be Map".into()),
                            };
                            let features = as_num_list(&a[1])?;
                            let weights = map_lookup(model, &Val::Str("weights".into()))
                                .ok_or("model missing weights")?;
                            let weights = as_num_list(weights)?;
                            let bias = map_lookup(model, &Val::Str("bias".into()))
                                .and_then(|v| if let Val::Num(n) = v { Some(*n) } else { None })
                                .ok_or("model missing bias")?;
                            if weights.len() != features.len() {
                                return Err("logreg_predict dimension mismatch".into());
                            }
                            let z = bias
                                + weights
                                    .iter()
                                    .zip(features.iter())
                                    .map(|(w, x)| w * x)
                                    .sum::<f64>();
                            Val::Num(1.0 / (1.0 + (-z).exp()))
                        }
                        "table_group_count" => {
                            if a.len() != 2 {
                                return Err("table_group_count expects rows,name".into());
                            }
                            let rows = match &a[0] {
                                Val::List(v) => v,
                                _ => return Err("rows must be List".into()),
                            };
                            let key = match &a[1] {
                                Val::Str(s) => Val::Str(s.clone()),
                                _ => return Err("name must be String".into()),
                            };
                            let mut groups: Vec<(Val, usize)> = vec![];
                            for row in rows {
                                let pairs = match row {
                                    Val::Map(p) => p,
                                    _ => return Err("rows must contain Maps".into()),
                                };
                                let value = map_lookup(pairs, &key).cloned().unwrap_or(Val::Null);
                                if let Some((_, n)) = groups.iter_mut().find(|(v, _)| *v == value) {
                                    *n += 1
                                } else {
                                    groups.push((value, 1))
                                }
                            }
                            Val::List(
                                groups
                                    .into_iter()
                                    .map(|(value, count)| {
                                        Val::Map(vec![
                                            (Val::Str("value".into()), value),
                                            (Val::Str("count".into()), Val::Num(count as f64)),
                                        ])
                                    })
                                    .collect(),
                            )
                        }
                        "table_group_sum" => {
                            if a.len() != 3 {
                                return Err("table_group_sum expects rows,group,value".into());
                            }
                            let rows = match &a[0] {
                                Val::List(v) => v,
                                _ => return Err("rows must be List".into()),
                            };
                            let gk = match &a[1] {
                                Val::Str(s) => Val::Str(s.clone()),
                                _ => return Err("group must be String".into()),
                            };
                            let vk = match &a[2] {
                                Val::Str(s) => Val::Str(s.clone()),
                                _ => return Err("value must be String".into()),
                            };
                            let mut groups: Vec<(Val, f64)> = vec![];
                            for row in rows {
                                let p = match row {
                                    Val::Map(p) => p,
                                    _ => return Err("rows must contain Maps".into()),
                                };
                                let group = map_lookup(p, &gk).cloned().unwrap_or(Val::Null);
                                let value = map_lookup(p, &vk)
                                    .and_then(|v| if let Val::Num(n) = v { Some(*n) } else { None })
                                    .ok_or("table_group_sum requires numeric value column")?;
                                if let Some((_, sum)) = groups.iter_mut().find(|(g, _)| *g == group)
                                {
                                    *sum += value
                                } else {
                                    groups.push((group, value));
                                }
                            }
                            Val::List(
                                groups
                                    .into_iter()
                                    .map(|(group, sum)| {
                                        Val::Map(vec![
                                            (Val::Str("value".into()), group),
                                            (Val::Str("sum".into()), Val::Num(sum)),
                                        ])
                                    })
                                    .collect(),
                            )
                        }
                        "table_group_mean" => {
                            if a.len() != 3 {
                                return Err("table_group_mean expects rows,group,value".into());
                            }
                            let rows = match &a[0] {
                                Val::List(v) => v,
                                _ => return Err("rows must be List".into()),
                            };
                            let gk = match &a[1] {
                                Val::Str(s) => Val::Str(s.clone()),
                                _ => return Err("group must be String".into()),
                            };
                            let vk = match &a[2] {
                                Val::Str(s) => Val::Str(s.clone()),
                                _ => return Err("value must be String".into()),
                            };
                            let mut groups: Vec<(Val, f64, usize)> = vec![];
                            for row in rows {
                                let p = match row {
                                    Val::Map(p) => p,
                                    _ => return Err("rows must contain Maps".into()),
                                };
                                let group = map_lookup(p, &gk).cloned().unwrap_or(Val::Null);
                                let value = map_lookup(p, &vk)
                                    .and_then(|v| if let Val::Num(n) = v { Some(*n) } else { None })
                                    .ok_or("table_group_mean requires numeric value column")?;
                                if let Some((_, sum, count)) =
                                    groups.iter_mut().find(|(g, _, _)| *g == group)
                                {
                                    *sum += value;
                                    *count += 1
                                } else {
                                    groups.push((group, value, 1));
                                }
                            }
                            Val::List(
                                groups
                                    .into_iter()
                                    .map(|(group, sum, count)| {
                                        Val::Map(vec![
                                            (Val::Str("value".into()), group),
                                            (Val::Str("mean".into()), Val::Num(sum / count as f64)),
                                            (Val::Str("count".into()), Val::Num(count as f64)),
                                        ])
                                    })
                                    .collect(),
                            )
                        }
                        "table_sort" => {
                            if a.len() != 2 {
                                return Err("table_sort expects rows,name".into());
                            }
                            let mut rows = match &a[0] {
                                Val::List(v) => v.clone(),
                                _ => return Err("rows must be List".into()),
                            };
                            let key = match &a[1] {
                                Val::Str(s) => Val::Str(s.clone()),
                                _ => return Err("name must be String".into()),
                            };
                            rows.sort_by(|ra, rb| {
                                let av = if let Val::Map(p) = ra {
                                    map_lookup(p, &key)
                                } else {
                                    None
                                };
                                let bv = if let Val::Map(p) = rb {
                                    map_lookup(p, &key)
                                } else {
                                    None
                                };
                                match (av, bv) {
                                    (Some(Val::Num(a)), Some(Val::Num(b))) => {
                                        a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)
                                    }
                                    (Some(Val::Str(a)), Some(Val::Str(b))) => a.cmp(b),
                                    _ => std::cmp::Ordering::Equal,
                                }
                            });
                            Val::List(rows)
                        }
                        "table_select" => {
                            if a.len() != 2 {
                                return Err("table_select expects rows,names".into());
                            }
                            let rows = match &a[0] {
                                Val::List(v) => v,
                                _ => return Err("rows must be List".into()),
                            };
                            let names = match &a[1] {
                                Val::List(v) => v,
                                _ => return Err("names must be List".into()),
                            };
                            let keys: Result<Vec<String>, String> = names
                                .iter()
                                .map(|v| match v {
                                    Val::Str(s) => Ok(s.clone()),
                                    _ => Err("column names must be Strings".into()),
                                })
                                .collect();
                            let keys = keys?;
                            let mut out = vec![];
                            for row in rows {
                                let pairs = match row {
                                    Val::Map(p) => p,
                                    _ => return Err("rows must contain Maps".into()),
                                };
                                let mut selected = vec![];
                                for name in &keys {
                                    let k = Val::Str(name.clone());
                                    selected.push((
                                        k.clone(),
                                        map_lookup(pairs, &k).cloned().unwrap_or(Val::Null),
                                    ));
                                }
                                out.push(Val::Map(selected));
                            }
                            Val::List(out)
                        }
                        "table_inner_join" => {
                            if a.len() != 4 {
                                return Err(
                                    "table_inner_join expects left,right,left_key,right_key".into(),
                                );
                            }
                            let left = match &a[0] {
                                Val::List(v) => v,
                                _ => return Err("left must be List".into()),
                            };
                            let right = match &a[1] {
                                Val::List(v) => v,
                                _ => return Err("right must be List".into()),
                            };
                            let lk = match &a[2] {
                                Val::Str(s) => Val::Str(s.clone()),
                                _ => return Err("left_key must be String".into()),
                            };
                            let rk = match &a[3] {
                                Val::Str(s) => Val::Str(s.clone()),
                                _ => return Err("right_key must be String".into()),
                            };
                            let mut out = vec![];
                            for lrow in left {
                                let lp = match lrow {
                                    Val::Map(p) => p,
                                    _ => return Err("left rows must be Maps".into()),
                                };
                                let lv = map_lookup(lp, &lk);
                                for rrow in right {
                                    let rp = match rrow {
                                        Val::Map(p) => p,
                                        _ => return Err("right rows must be Maps".into()),
                                    };
                                    if lv.is_some() && lv == map_lookup(rp, &rk) {
                                        let mut merged = lp.clone();
                                        for (k, v) in rp {
                                            let key =
                                                if merged.iter().any(|(existing, _)| existing == k)
                                                {
                                                    Val::Str(format!("right.{}", k.show()))
                                                } else {
                                                    k.clone()
                                                };
                                            merged.push((key, v.clone()));
                                        }
                                        out.push(Val::Map(merged));
                                    }
                                }
                            }
                            Val::List(out)
                        }
                        "table_head" => {
                            if a.len() != 2 {
                                return Err("table_head expects rows,count".into());
                            }
                            let rows = match &a[0] {
                                Val::List(v) => v,
                                _ => return Err("rows must be List".into()),
                            };
                            let n = match a[1] {
                                Val::Num(n) => n.max(0.0) as usize,
                                _ => return Err("count must be Number".into()),
                            };
                            Val::List(rows.iter().take(n).cloned().collect())
                        }
                        "table_tail" => {
                            if a.len() != 2 {
                                return Err("table_tail expects rows,count".into());
                            }
                            let rows = match &a[0] {
                                Val::List(v) => v,
                                _ => return Err("rows must be List".into()),
                            };
                            let n = match a[1] {
                                Val::Num(n) => n.max(0.0) as usize,
                                _ => return Err("count must be Number".into()),
                            };
                            let start = rows.len().saturating_sub(n);
                            Val::List(rows[start..].to_vec())
                        }
                        "table_drop_nulls" => {
                            if a.len() != 2 {
                                return Err("table_drop_nulls expects rows,column".into());
                            }
                            let rows = match &a[0] {
                                Val::List(v) => v,
                                _ => return Err("rows must be List".into()),
                            };
                            let key = match &a[1] {
                                Val::Str(s) => Val::Str(s.clone()),
                                _ => return Err("column must be String".into()),
                            };
                            Val::List(
                                rows.iter()
                                    .filter(|row| match row {
                                        Val::Map(p) => map_lookup(p, &key)
                                            .map(|v| !matches!(v, Val::Null))
                                            .unwrap_or(false),
                                        _ => false,
                                    })
                                    .cloned()
                                    .collect(),
                            )
                        }
                        "table_fill_null" => {
                            if a.len() != 3 {
                                return Err("table_fill_null expects rows,column,value".into());
                            }
                            let rows = match &a[0] {
                                Val::List(v) => v,
                                _ => return Err("rows must be List".into()),
                            };
                            let key = match &a[1] {
                                Val::Str(s) => Val::Str(s.clone()),
                                _ => return Err("column must be String".into()),
                            };
                            let fill = a[2].clone();
                            let mut out = vec![];
                            for row in rows {
                                let mut pairs = match row {
                                    Val::Map(p) => p.clone(),
                                    _ => return Err("rows must contain Maps".into()),
                                };
                                if let Some((_, v)) = pairs.iter_mut().find(|(k, _)| *k == key) {
                                    if matches!(v, Val::Null) {
                                        *v = fill.clone();
                                    }
                                } else {
                                    pairs.push((key.clone(), fill.clone()));
                                }
                                out.push(Val::Map(pairs));
                            }
                            Val::List(out)
                        }
                        "table_left_join" => {
                            if a.len() != 4 {
                                return Err(
                                    "table_left_join expects left,right,left_key,right_key".into(),
                                );
                            }
                            let left = match &a[0] {
                                Val::List(v) => v,
                                _ => return Err("left must be List".into()),
                            };
                            let right = match &a[1] {
                                Val::List(v) => v,
                                _ => return Err("right must be List".into()),
                            };
                            let lk = match &a[2] {
                                Val::Str(s) => Val::Str(s.clone()),
                                _ => return Err("left_key must be String".into()),
                            };
                            let rk = match &a[3] {
                                Val::Str(s) => Val::Str(s.clone()),
                                _ => return Err("right_key must be String".into()),
                            };
                            let mut out = vec![];
                            for lrow in left {
                                let lp = match lrow {
                                    Val::Map(p) => p,
                                    _ => return Err("left rows must be Maps".into()),
                                };
                                let lv = map_lookup(lp, &lk);
                                let mut matched = false;
                                for rrow in right {
                                    let rp = match rrow {
                                        Val::Map(p) => p,
                                        _ => return Err("right rows must be Maps".into()),
                                    };
                                    if lv.is_some() && lv == map_lookup(rp, &rk) {
                                        matched = true;
                                        let mut merged = lp.clone();
                                        for (k, v) in rp {
                                            let key =
                                                if merged.iter().any(|(existing, _)| existing == k)
                                                {
                                                    Val::Str(format!("right.{}", k.show()))
                                                } else {
                                                    k.clone()
                                                };
                                            merged.push((key, v.clone()));
                                        }
                                        out.push(Val::Map(merged));
                                    }
                                }
                                if !matched {
                                    out.push(Val::Map(lp.clone()));
                                }
                            }
                            Val::List(out)
                        }
                        "csv_read" => {
                            if a.len() != 1 {
                                return Err("csv_read expects path".into());
                            }
                            let p = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("path must be String".into()),
                            };
                            let mut rdr = csv::Reader::from_path(p).map_err(|e| e.to_string())?;
                            let headers = rdr
                                .headers()
                                .map_err(|e| e.to_string())?
                                .iter()
                                .map(|s| s.to_string())
                                .collect::<Vec<_>>();
                            let mut rows = vec![];
                            for rec in rdr.records() {
                                let rec = rec.map_err(|e| e.to_string())?;
                                let pairs = headers
                                    .iter()
                                    .zip(rec.iter())
                                    .map(|(h, v)| {
                                        let val = v
                                            .parse::<f64>()
                                            .map(Val::Num)
                                            .unwrap_or_else(|_| Val::Str(v.to_string()));
                                        (Val::Str(h.clone()), val)
                                    })
                                    .collect();
                                rows.push(Val::Map(pairs));
                            }
                            Val::List(rows)
                        }
                        "csv_write" => {
                            if a.len() != 2 {
                                return Err("csv_write expects path,rows".into());
                            }
                            let p = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("path must be String".into()),
                            };
                            let rows = match &a[1] {
                                Val::List(v) => v,
                                _ => return Err("rows must be List".into()),
                            };
                            let mut w = csv::Writer::from_path(p).map_err(|e| e.to_string())?;
                            if let Some(Val::Map(first)) = rows.first() {
                                let headers =
                                    first.iter().map(|(k, _)| k.show()).collect::<Vec<_>>();
                                w.write_record(&headers).map_err(|e| e.to_string())?;
                                for row in rows {
                                    let map = match row {
                                        Val::Map(p) => p,
                                        _ => return Err("all csv rows must be Map".into()),
                                    };
                                    let vals = headers
                                        .iter()
                                        .map(|h| {
                                            map.iter()
                                                .find(|(k, _)| k.show() == *h)
                                                .map(|(_, v)| v.show())
                                                .unwrap_or_default()
                                        })
                                        .collect::<Vec<_>>();
                                    w.write_record(vals).map_err(|e| e.to_string())?;
                                }
                            }
                            w.flush().map_err(|e| e.to_string())?;
                            Val::Null
                        }
                        #[cfg(feature = "native-services")]
                        "gui_message" => {
                            if a.len() != 2 {
                                return Err("gui_message expects title,message".into());
                            }
                            let title = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("title must be String".into()),
                            };
                            let msg = match &a[1] {
                                Val::Str(s) => s,
                                _ => return Err("message must be String".into()),
                            };
                            tinyfiledialogs::message_box_ok(
                                title,
                                msg,
                                tinyfiledialogs::MessageBoxIcon::Info,
                            );
                            Val::Null
                        }
                        #[cfg(feature = "native-services")]
                        "gui_input" => {
                            if a.len() != 2 {
                                return Err("gui_input expects title,message".into());
                            }
                            let title = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("title must be String".into()),
                            };
                            let msg = match &a[1] {
                                Val::Str(s) => s,
                                _ => return Err("message must be String".into()),
                            };
                            tinyfiledialogs::input_box(title, msg, "")
                                .map(Val::Str)
                                .unwrap_or(Val::Null)
                        }
                        #[cfg(feature = "native-services")]
                        "gui_open_file" => {
                            if a.len() != 1 {
                                return Err("gui_open_file expects title".into());
                            }
                            let title = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("title must be String".into()),
                            };
                            tinyfiledialogs::open_file_dialog(title, "", None)
                                .map(Val::Str)
                                .unwrap_or(Val::Null)
                        }
                        #[cfg(feature = "native-services")]
                        "gui_save_file" => {
                            if a.len() != 1 {
                                return Err("gui_save_file expects title".into());
                            }
                            let title = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("title must be String".into()),
                            };
                            tinyfiledialogs::save_file_dialog(title, "")
                                .map(Val::Str)
                                .unwrap_or(Val::Null)
                        }
                        #[cfg(feature = "native-services")]
                        "gui_confirm" => {
                            if a.len() != 2 {
                                return Err("gui_confirm expects title,message".into());
                            }
                            let title = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("title must be String".into()),
                            };
                            let msg = match &a[1] {
                                Val::Str(s) => s,
                                _ => return Err("message must be String".into()),
                            };
                            Val::Bool(matches!(
                                tinyfiledialogs::message_box_yes_no(
                                    title,
                                    msg,
                                    tinyfiledialogs::MessageBoxIcon::Question,
                                    tinyfiledialogs::YesNo::Yes
                                ),
                                tinyfiledialogs::YesNo::Yes
                            ))
                        }
                        #[cfg(feature = "native-services")]
                        "gui_password" => {
                            if a.len() != 2 {
                                return Err("gui_password expects title,message".into());
                            }
                            let title = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("title must be String".into()),
                            };
                            let msg = match &a[1] {
                                Val::Str(s) => s,
                                _ => return Err("message must be String".into()),
                            };
                            tinyfiledialogs::password_box(title, msg)
                                .map(Val::Str)
                                .unwrap_or(Val::Null)
                        }
                        #[cfg(feature = "native-services")]
                        "gui_folder" => {
                            if a.len() != 1 {
                                return Err("gui_folder expects title".into());
                            }
                            let title = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("title must be String".into()),
                            };
                            tinyfiledialogs::select_folder_dialog(title, "")
                                .map(Val::Str)
                                .unwrap_or(Val::Null)
                        }
                        #[cfg(feature = "native-services")]
                        "gui_color" => {
                            if a.len() != 1 {
                                return Err("gui_color expects title".into());
                            }
                            let title = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("title must be String".into()),
                            };
                            match tinyfiledialogs::color_chooser_dialog(
                                title,
                                tinyfiledialogs::DefaultColorValue::Hex("#ffffff"),
                            ) {
                                Some((hex, _)) => Val::Str(hex),
                                None => Val::Null,
                            }
                        }
                        #[cfg(feature = "native-services")]
                        "gui_form" => {
                            if a.len() != 2 {
                                return Err("gui_form expects title,fields".into());
                            }
                            let title = match &a[0] {
                                Val::Str(s) => s,
                                _ => return Err("title must be String".into()),
                            };
                            let fields = match &a[1] {
                                Val::List(v) => v,
                                _ => return Err("fields must be List".into()),
                            };
                            let mut out = vec![];
                            for field in fields {
                                let label = match field {
                                    Val::Str(s) => s,
                                    _ => return Err("field names must be String".into()),
                                };
                                match tinyfiledialogs::input_box(title, label, "") {
                                    Some(value) => {
                                        out.push((Val::Str(label.clone()), Val::Str(value)))
                                    }
                                    None => return Ok(Val::Null),
                                }
                            }
                            Val::Map(out)
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
                        "pow" => {
                            if a.len() != 2 {
                                return Err("pow expects base,exponent".into());
                            }
                            match (a[0].clone(), a[1].clone()) {
                                (Val::Num(x), Val::Num(y)) => Val::Num(x.powf(y)),
                                _ => return Err("pow expects Numbers".into()),
                            }
                        }
                        "round" => {
                            if a.len() != 1 {
                                return Err("round expects number".into());
                            }
                            match a[0] {
                                Val::Num(n) => Val::Num(n.round()),
                                _ => return Err("round expects Number".into()),
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
                            Val::Str(
                                base64::engine::general_purpose::STANDARD
                                    .encode(a[0].show().as_bytes()),
                            )
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
                            let parsed: toml::Value =
                                toml::from_str(text).map_err(|e| e.to_string())?;
                            let json = serde_json::to_value(parsed).map_err(|e| e.to_string())?;
                            json_to_val(&json)
                        }
                        "toml_stringify" => {
                            if a.len() != 1 {
                                return Err("toml_stringify expects value".into());
                            }
                            Val::Str(
                                toml::to_string(&val_to_json(&a[0])).map_err(|e| e.to_string())?,
                            )
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
                                return Err(
                                    "pbkdf2_sha256 expects password,salt,iterations,bytes".into()
                                );
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
                                return Err(
                                    "http_request expects method,url,headers,body,timeout_ms"
                                        .into(),
                                );
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
                                            rusqlite::types::ValueRef::Integer(x) => {
                                                Val::Num(x as f64)
                                            }
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
                                            rusqlite::types::ValueRef::Integer(x) => {
                                                Val::Num(x as f64)
                                            }
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
                                let mut vars = def.captures.clone();
                                let mut var_types = def.capture_types.clone();
                                let return_type = def.return_type.clone();
                                for ((param, ty), value) in
                                    def.params.into_iter().zip(args.into_iter())
                                {
                                    if let Some(expected) = ty {
                                        var_types.insert(param.clone(), expected);
                                    }
                                    vars.insert(param, value);
                                }
                                let mut nested =
                                    Vm::nested(def.code, vars, funcs, classes, objects);
                                nested.var_types = var_types;
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
                                    let guard =
                                        tasks().lock().map_err(|_| "task registry poisoned")?;
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
                                    break handle
                                        .join()
                                        .map_err(|_| "task panicked".to_string())??;
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
                                        let f: libloading::Symbol<
                                            unsafe extern "C" fn(i32) -> i32,
                                        > = lib.get(&sym).map_err(|e| e.to_string())?;
                                        f(*a)
                                    }
                                    [a, b] => {
                                        let f: libloading::Symbol<
                                            unsafe extern "C" fn(i32, i32) -> i32,
                                        > = lib.get(&sym).map_err(|e| e.to_string())?;
                                        f(*a, *b)
                                    }
                                    [a, b, c] => {
                                        let f: libloading::Symbol<
                                            unsafe extern "C" fn(i32, i32, i32) -> i32,
                                        > = lib.get(&sym).map_err(|e| e.to_string())?;
                                        f(*a, *b, *c)
                                    }
                                    [a, b, c, e] => {
                                        let f: libloading::Symbol<
                                            unsafe extern "C" fn(i32, i32, i32, i32) -> i32,
                                        > = lib.get(&sym).map_err(|e| e.to_string())?;
                                        f(*a, *b, *c, *e)
                                    }
                                    _ => {
                                        return Err(
                                            "ffi_i32 supports 0..4 integer arguments".into()
                                        );
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
                                        let f: libloading::Symbol<
                                            unsafe extern "C" fn(i64) -> i64,
                                        > = lib.get(&sym).map_err(|e| e.to_string())?;
                                        f(*a)
                                    }
                                    [a, b] => {
                                        let f: libloading::Symbol<
                                            unsafe extern "C" fn(i64, i64) -> i64,
                                        > = lib.get(&sym).map_err(|e| e.to_string())?;
                                        f(*a, *b)
                                    }
                                    _ => {
                                        return Err(
                                            "ffi_i64 supports 0..2 integer arguments".into()
                                        );
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
                                        let f: libloading::Symbol<
                                            unsafe extern "C" fn(f64) -> f64,
                                        > = lib.get(&sym).map_err(|e| e.to_string())?;
                                        f(*a)
                                    }
                                    [a, b] => {
                                        let f: libloading::Symbol<
                                            unsafe extern "C" fn(f64, f64) -> f64,
                                        > = lib.get(&sym).map_err(|e| e.to_string())?;
                                        f(*a, *b)
                                    }
                                    _ => {
                                        return Err(
                                            "ffi_f64 supports 0..2 floating arguments".into()
                                        );
                                    }
                                }
                            };
                            Val::Num(result)
                        }
                        _ => {
                            if self.classes.contains_key(&n) {
                                self.instantiate(&n, a)?
                            } else if let Some(Val::Function(target)) = self.vars.get(&n).cloned() {
                                let def = if let Some(def) = self.funcs.get(&target).cloned() {
                                    def
                                } else if let Some(def) = resolve_function_object(&target)? {
                                    def
                                } else {
                                    return Err(format!("dangling function reference '{target}'"));
                                };
                                self.call_def(def, a)?
                            } else {
                                let def = self
                                    .funcs
                                    .get(&n)
                                    .cloned()
                                    .ok_or(format!("unknown function or class '{n}'"))?;
                                self.call_def(def, a)?
                            }
                        }
                    };
                    self.stack.push(v);
                }
                Op::FunctionRef(name) => {
                    let def = self
                        .funcs
                        .get(&name)
                        .cloned()
                        .ok_or(format!("unknown function reference '{name}'"))?;
                    let key = export_function_object(def)?;
                    self.stack.push(Val::Function(key));
                }
                Op::Def(name, params, return_type, is_async, code) => {
                    self.funcs.insert(
                        name,
                        FunctionDef {
                            params,
                            return_type,
                            is_async,
                            code,
                            captures: self.vars.clone(),
                            capture_types: self.var_types.clone(),
                        },
                    );
                }
                Op::DefClass(name, parent, methods) => {
                    if let Some(ref p) = parent {
                        if !self.classes.contains_key(p) {
                            return Err(format!("unknown parent class '{p}'"));
                        }
                    }
                    let mut compiled = HashMap::new();
                    for (method, params, return_type, is_async, code) in methods {
                        compiled.insert(
                            method,
                            FunctionDef {
                                params,
                                return_type,
                                is_async,
                                code,
                                captures: self.vars.clone(),
                                capture_types: self.var_types.clone(),
                            },
                        );
                    }
                    self.classes.insert(
                        name,
                        ClassDef {
                            parent,
                            methods: compiled,
                        },
                    );
                }
                Op::Return => {
                    self.finished = true;
                    return self.pop();
                }
                Op::Yield => return self.pop(),
                Op::PushHandler(target, name) => self.handlers.push((target, name)),
                Op::PopHandler => {
                    let _ = self.handlers.pop();
                }
                Op::Throw => {
                    let value = self.pop()?;
                    if let Some((target, name)) = self.handlers.pop() {
                        self.vars.insert(name, value);
                        self.ip = target;
                    } else {
                        return Err(format!("uncaught throw: {}", value.show()));
                    }
                }
                Op::Halt => {
                    self.finished = true;
                    return Ok(Val::Null);
                }
            }
        }
    }
}
