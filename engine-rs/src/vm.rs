// The stack virtual machine: executes the compiled `Op` stream.
//
// Extracted from `main.rs` without behavior change; see docs/ENGINE-SPLIT.md.
// `execute()` is still one function with one giant match; separating the 212
// inline builtins out of `Op::Call` is Phase 2 of the split.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, OnceLock, atomic::Ordering},
    thread,
};

use crate::bytecode::{ClassDef, FunctionDef, ObjectData, Op};
use crate::runtime::{
    GENERATOR_IDS, OBJECT_IDS, TASK_IDS, export_function_object, resolve_function_object, tasks,
};
use crate::tensor::{collect_val_tensor_ids, tensor_gc_unreachable};
use crate::value::Val;

pub(crate) struct Vm {
    pub(crate) code: Arc<Vec<Op>>,
    pub(crate) ip: usize,
    pub(crate) stack: Vec<Val>,
    pub(crate) vars: HashMap<String, Val>,
    pub(crate) cells: Arc<HashMap<String, Arc<Mutex<Val>>>>,
    pub(crate) var_types: Arc<HashMap<String, String>>,
    pub(crate) consts: HashSet<String>,
    pub(crate) funcs: Arc<HashMap<String, FunctionDef>>,
    pub(crate) classes: Arc<HashMap<String, ClassDef>>,
    pub(crate) objects: Arc<Mutex<HashMap<u64, ObjectData>>>,
    pub(crate) handlers: Vec<(usize, String, usize)>,
    pub(crate) trace: bool,
    pub(crate) finished: bool,
}

static GENERATORS: OnceLock<Mutex<HashMap<u64, Vm>>> = OnceLock::new();

pub(crate) fn generators() -> &'static Mutex<HashMap<u64, Vm>> {
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
            code: Arc::new(code),
            ip: 0,
            stack: vec![],
            vars: HashMap::new(),
            cells: Arc::new(HashMap::new()),
            var_types: Arc::new(HashMap::new()),
            consts: HashSet::new(),
            funcs: Arc::new(HashMap::new()),
            classes: Arc::new(HashMap::new()),
            objects: Arc::new(Mutex::new(HashMap::new())),
            handlers: vec![],
            trace: false,
            finished: false,
        }
    }
    pub(crate) fn nested(
        code: Arc<Vec<Op>>,
        vars: HashMap<String, Val>,
        funcs: Arc<HashMap<String, FunctionDef>>,
        classes: Arc<HashMap<String, ClassDef>>,
        objects: Arc<Mutex<HashMap<u64, ObjectData>>>,
    ) -> Self {
        Self {
            code,
            ip: 0,
            stack: vec![],
            vars,
            cells: Arc::new(HashMap::new()),
            var_types: Arc::new(HashMap::new()),
            consts: HashSet::new(),
            funcs,
            classes,
            objects,
            handlers: vec![],
            trace: false,
            finished: false,
        }
    }
    fn get_var(&self, name: &str) -> Option<Val> {
        if let Some(cell) = self.cells.get(name) {
            cell.lock().ok().map(|g| g.clone())
        } else {
            self.vars.get(name).cloned()
        }
    }
    fn set_var(&mut self, name: String, value: Val) {
        if let Some(cell) = self.cells.get(&name) {
            if let Ok(mut guard) = cell.lock() {
                *guard = value.clone();
            }
        }
        self.vars.insert(name, value);
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
    pub(crate) fn value_matches(&self, value: &Val, ty: &str) -> bool {
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
    pub(crate) fn call_def(&self, def: FunctionDef, args: Vec<Val>) -> Result<Val, String> {
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
        let mut vars = if def.captures.is_empty() {
            HashMap::with_capacity(def.params.len())
        } else {
            (*def.captures).clone()
        };
        let mut cells = def.upvalues.clone();
        let mut var_types = def.capture_types.clone();
        let return_type = def.return_type.clone();
        for ((param, ty), value) in def.params.iter().zip(args) {
            if let Some(expected) = ty {
                if !self.value_matches(&value, expected) {
                    return Err(format!(
                        "argument '{param}' expected {expected}, got {}",
                        value.show()
                    ));
                }
                Arc::make_mut(&mut var_types).insert(param.clone(), expected.clone());
            }
            if cells.contains_key(param) {
                Arc::make_mut(&mut cells).remove(param);
            }
            vars.insert(param.clone(), value);
        }
        let mut nested = Vm::nested(
            def.code,
            vars,
            self.funcs.clone(),
            self.classes.clone(),
            self.objects.clone(),
        );
        nested.var_types = var_types;
        nested.cells = cells;
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
    pub(crate) fn instantiate(&self, class_name: &str, args: Vec<Val>) -> Result<Val, String> {
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
    pub(crate) fn callable_def(&self, value: &Val) -> Result<(String, FunctionDef), String> {
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
    pub(crate) fn print_val(&self, args: &[Val]) -> Result<Val, String> {
        println!(
            "{}",
            args.iter().map(Val::show).collect::<Vec<_>>().join(" ")
        );
        Ok(Val::Null)
    }
    pub(crate) fn len_val(&self, args: &[Val]) -> Result<Val, String> {
        if args.len() != 1 {
            return Err("len expects 1 argument".into());
        }
        Ok(match &args[0] {
            Val::Str(s) => Val::Num(s.chars().count() as f64),
            Val::List(v) => Val::Num(v.len() as f64),
            Val::Map(v) => Val::Num(v.len() as f64),
            _ => return Err("len supports String/List/Map".into()),
        })
    }
    pub(crate) fn type_of_val(&self, args: &[Val]) -> Result<Val, String> {
        if args.len() != 1 {
            return Err("type_of expects 1 argument".into());
        }
        Ok(match &args[0] {
            Val::Num(n) => Val::Str(if n.fract() == 0.0 {
                "Int".to_string()
            } else {
                "Number".to_string()
            }),
            Val::Str(_) => Val::Str("String".to_string()),
            Val::Bool(_) => Val::Str("Bool".to_string()),
            Val::Null => Val::Str("Null".to_string()),
            Val::List(_) => Val::Str("List".to_string()),
            Val::Map(_) => Val::Str("Map".to_string()),
            Val::Function(_) => Val::Str("Function".to_string()),
            Val::Generator(_) => Val::Str("Generator".to_string()),
            Val::Object(id) => {
                let heap = self.objects.lock().map_err(|_| "object heap poisoned")?;
                Val::Str(
                    heap.get(id)
                        .map(|o| o.class_name.clone())
                        .unwrap_or_else(|| "Object".into()),
                )
            }
        })
    }
    pub(crate) fn gc_tensors(&self) -> Result<usize, String> {
        let mut live = HashSet::new();
        for v in self.vars.values() {
            collect_val_tensor_ids(v, &mut live);
        }
        for cell in self.cells.values() {
            if let Ok(guard) = cell.lock() {
                collect_val_tensor_ids(&guard, &mut live);
            }
        }
        for v in &self.stack {
            collect_val_tensor_ids(v, &mut live);
        }
        tensor_gc_unreachable(&live)
    }
    pub(crate) fn execute(&mut self) -> Result<Val, String> {
        loop {
            if !self.trace {
                if self.cells.is_empty() {
                    if let Some(window) = self.code.get(self.ip..self.ip + 4) {
                        match window {
                            [
                                Op::Load(a),
                                Op::Load(b),
                                cmp @ (Op::Lt | Op::Le | Op::Gt | Op::Ge),
                                Op::JumpIfFalse(target),
                            ] => {
                                if let (Some(Val::Num(x)), Some(Val::Num(y))) =
                                    (self.vars.get(a), self.vars.get(b))
                                {
                                    let yes = match cmp {
                                        Op::Lt => *x < *y,
                                        Op::Le => *x <= *y,
                                        Op::Gt => *x > *y,
                                        _ => *x >= *y,
                                    };
                                    self.ip = if yes { self.ip + 4 } else { *target };
                                    continue;
                                }
                            }
                            [
                                Op::Load(a),
                                Op::Const(Val::Num(y)),
                                cmp @ (Op::Lt | Op::Le | Op::Gt | Op::Ge),
                                Op::JumpIfFalse(target),
                            ] => {
                                if let Some(Val::Num(x)) = self.vars.get(a) {
                                    let yes = match cmp {
                                        Op::Lt => *x < *y,
                                        Op::Le => *x <= *y,
                                        Op::Gt => *x > *y,
                                        _ => *x >= *y,
                                    };
                                    self.ip = if yes { self.ip + 4 } else { *target };
                                    continue;
                                }
                            }
                            [
                                Op::Load(a),
                                Op::Const(Val::Num(k)),
                                op @ (Op::Add | Op::Sub | Op::Mul),
                                Op::Store(s),
                            ] if a == s
                                && !self.consts.contains(a)
                                && !self.var_types.contains_key(a) =>
                            {
                                let rhs = *k;
                                let is_add = matches!(op, Op::Add);
                                let is_sub = matches!(op, Op::Sub);
                                if let Some(Val::Num(x)) = self.vars.get_mut(a) {
                                    *x = if is_add {
                                        *x + rhs
                                    } else if is_sub {
                                        *x - rhs
                                    } else {
                                        *x * rhs
                                    };
                                    self.ip += 4;
                                    continue;
                                }
                            }
                            [
                                Op::Load(a),
                                Op::Load(b),
                                op @ (Op::Add | Op::Sub | Op::Mul),
                                Op::Store(s),
                            ] if a == s
                                && !self.consts.contains(a)
                                && !self.var_types.contains_key(a) =>
                            {
                                if let Some(Val::Num(y)) = self.vars.get(b).cloned() {
                                    let is_add = matches!(op, Op::Add);
                                    let is_sub = matches!(op, Op::Sub);
                                    if let Some(Val::Num(x)) = self.vars.get_mut(a) {
                                        *x = if is_add {
                                            *x + y
                                        } else if is_sub {
                                            *x - y
                                        } else {
                                            *x * y
                                        };
                                        self.ip += 4;
                                        continue;
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                match self.code.get(self.ip) {
                    Some(Op::Jump(target)) => {
                        self.ip = *target;
                        continue;
                    }
                    Some(Op::Const(Val::Num(n))) => {
                        self.stack.push(Val::Num(*n));
                        self.ip += 1;
                        continue;
                    }
                    Some(Op::Const(Val::Bool(b))) => {
                        self.stack.push(Val::Bool(*b));
                        self.ip += 1;
                        continue;
                    }
                    Some(Op::Const(Val::Null)) => {
                        self.stack.push(Val::Null);
                        self.ip += 1;
                        continue;
                    }
                    _ => {}
                }
            }
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
            match self.step_op(op) {
                Ok(Some(v)) => return Ok(v),
                Ok(None) => {}
                Err(err) => {
                    if let Some((target, name, depth)) = self.handlers.pop() {
                        self.stack.truncate(depth);
                        let msg = err
                            .strip_prefix("uncaught throw: ")
                            .unwrap_or(&err)
                            .to_string();
                        self.set_var(name, Val::Str(msg));
                        self.ip = target;
                    } else {
                        return Err(err);
                    }
                }
            }
        }
    }
    fn step_op(&mut self, op: Op) -> Result<Option<Val>, String> {
        match op {
            Op::Const(v) => self.stack.push(v),
            Op::Load(n) => {
                if let (Some(Op::Load(idx_name)), Some(Op::Index)) =
                    (self.code.get(self.ip), self.code.get(self.ip + 1))
                {
                    if let Some(Val::Num(idx_num)) = self.get_var(idx_name) {
                        if idx_num >= 0.0 {
                            let i = idx_num as usize;
                            let indexed = if let Some(cell) = self.cells.get(&n) {
                                let guard = cell
                                    .lock()
                                    .map_err(|_| "upvalue cell poisoned".to_string())?;
                                match &*guard {
                                    Val::List(v) => {
                                        Some(v.get(i).cloned().ok_or_else(|| {
                                            format!("list index {i} out of range")
                                        })?)
                                    }
                                    _ => None,
                                }
                            } else if let Some(Val::List(v)) = self.vars.get(&n) {
                                Some(
                                    v.get(i)
                                        .cloned()
                                        .ok_or_else(|| format!("list index {i} out of range"))?,
                                )
                            } else {
                                None
                            };
                            if let Some(val) = indexed {
                                self.ip += 2;
                                self.stack.push(val);
                                return Ok(None);
                            }
                        }
                    }
                }
                if let Some(Op::Call(fn_name, 1)) = self.code.get(self.ip) {
                    if fn_name == "len" {
                        let len_opt = if let Some(cell) = self.cells.get(&n) {
                            let guard = cell
                                .lock()
                                .map_err(|_| "upvalue cell poisoned".to_string())?;
                            match &*guard {
                                Val::List(v) => Some(v.len()),
                                Val::Str(s) => Some(s.chars().count()),
                                Val::Map(m) => Some(m.len()),
                                _ => None,
                            }
                        } else {
                            match self.vars.get(&n) {
                                Some(Val::List(v)) => Some(v.len()),
                                Some(Val::Str(s)) => Some(s.chars().count()),
                                Some(Val::Map(m)) => Some(m.len()),
                                _ => None,
                            }
                        };
                        if let Some(len) = len_opt {
                            self.ip += 1;
                            self.stack.push(Val::Num(len as f64));
                            return Ok(None);
                        }
                    }
                }
                let value = if let Some(v) = self.get_var(&n) {
                    v
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
                    Arc::make_mut(&mut self.var_types).insert(n.clone(), expected.clone());
                }
                if self.cells.contains_key(&n) {
                    Arc::make_mut(&mut self.cells).remove(&n);
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
                self.set_var(n, v);
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
            Op::Pow => self.bin_num(|a, b| a.powf(b))?,
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
            Op::Index => {
                let key = self.pop()?;
                let obj = self.pop()?;
                let value = match (obj, key) {
                    (Val::List(v), Val::Num(n)) => {
                        let raw = n as isize;
                        let len = v.len() as isize;
                        let resolved = if raw < 0 { len + raw } else { raw };
                        if resolved < 0 || (resolved as usize) >= v.len() {
                            return Err(format!("list index {raw} out of range"));
                        }
                        v[resolved as usize].clone()
                    }
                    (Val::Str(s), Val::Num(n)) => {
                        let chars: Vec<char> = s.chars().collect();
                        let raw = n as isize;
                        let len = chars.len() as isize;
                        let resolved = if raw < 0 { len + raw } else { raw };
                        if resolved < 0 || (resolved as usize) >= chars.len() {
                            return Err(format!("string index {raw} out of range"));
                        }
                        Val::Str(chars[resolved as usize].to_string())
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
            Op::SetIndex => {
                let value = self.pop()?;
                let key = self.pop()?;
                let obj = self.pop()?;
                let updated = match (obj, key) {
                    (Val::List(mut items), Val::Num(n)) => {
                        if n.fract() != 0.0 {
                            return Err("list index must be integer".into());
                        }
                        let raw = n as isize;
                        let len = items.len() as isize;
                        let resolved = if raw < 0 { len + raw } else { raw };
                        if resolved < 0 || (resolved as usize) >= items.len() {
                            return Err(format!("list index {raw} out of range"));
                        }
                        items[resolved as usize] = value;
                        Val::List(items)
                    }
                    (Val::Map(mut pairs), k) => {
                        if let Some((_, existing)) = pairs.iter_mut().find(|(ek, _)| *ek == k) {
                            *existing = value;
                        } else {
                            pairs.push((k, value));
                        }
                        Val::Map(pairs)
                    }
                    _ => return Err("index assignment requires List or Map".into()),
                };
                self.stack.push(updated);
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
                        let mut heap = self.objects.lock().map_err(|_| "object heap poisoned")?;
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
                            let heap = self.objects.lock().map_err(|_| "object heap poisoned")?;
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
                        if let Some((_, member)) =
                            pairs.iter().find(|(k, _)| *k == Val::Str(name.clone()))
                        {
                            if let Ok((_, def)) = self.callable_def(member) {
                                self.stack.push(self.call_def(def, args)?);
                                return Ok(None);
                            }
                        }
                        match name.as_str() {
                            "keys" => Val::List(pairs.iter().map(|(k, _)| k.clone()).collect()),
                            "values" => Val::List(pairs.iter().map(|(_, v)| v.clone()).collect()),
                            "items" | "entries" => Val::List(
                                pairs
                                    .iter()
                                    .map(|(k, v)| Val::List(vec![k.clone(), v.clone()]))
                                    .collect(),
                            ),
                            "get" => {
                                if args.is_empty() || args.len() > 2 {
                                    return Err("map.get expects (key[, default])".into());
                                }
                                pairs
                                    .iter()
                                    .find(|(k, _)| *k == args[0])
                                    .map(|(_, v)| v.clone())
                                    .unwrap_or_else(|| args.get(1).cloned().unwrap_or(Val::Null))
                            }
                            "has" | "contains" => {
                                if args.len() != 1 {
                                    return Err("map.has expects 1 argument".into());
                                }
                                Val::Bool(pairs.iter().any(|(k, _)| *k == args[0]))
                            }
                            "len" => Val::Num(pairs.len() as f64),
                            _ => return Err(format!("undefined module member '{name}'")),
                        }
                    }
                    Val::Str(s) => match name.as_str() {
                        "upper" => Val::Str(s.to_uppercase()),
                        "lower" => Val::Str(s.to_lowercase()),
                        "strip" | "trim" => Val::Str(s.trim().to_string()),
                        "lstrip" => Val::Str(s.trim_start().to_string()),
                        "rstrip" => Val::Str(s.trim_end().to_string()),
                        "split" => {
                            if args.is_empty() {
                                Val::List(
                                    s.split_whitespace()
                                        .map(|p| Val::Str(p.to_string()))
                                        .collect(),
                                )
                            } else if let Val::Str(sep) = &args[0] {
                                Val::List(s.split(sep).map(|p| Val::Str(p.to_string())).collect())
                            } else {
                                return Err("str.split separator must be String".into());
                            }
                        }
                        "replace" => {
                            if args.len() != 2 {
                                return Err("str.replace expects (old, new)".into());
                            }
                            match (&args[0], &args[1]) {
                                (Val::Str(old), Val::Str(new)) => Val::Str(s.replace(old, new)),
                                _ => return Err("str.replace arguments must be Strings".into()),
                            }
                        }
                        "startswith" | "starts_with" => match args.first() {
                            Some(Val::Str(p)) => Val::Bool(s.starts_with(p)),
                            _ => return Err("str.startswith expects String".into()),
                        },
                        "endswith" | "ends_with" => match args.first() {
                            Some(Val::Str(p)) => Val::Bool(s.ends_with(p)),
                            _ => return Err("str.endswith expects String".into()),
                        },
                        "find" => match args.first() {
                            Some(Val::Str(sub)) => {
                                Val::Num(s.find(sub).map(|i| i as f64).unwrap_or(-1.0))
                            }
                            _ => return Err("str.find expects String".into()),
                        },
                        "count" => match args.first() {
                            Some(Val::Str(sub)) => Val::Num(s.matches(sub).count() as f64),
                            _ => return Err("str.count expects String".into()),
                        },
                        "join" => match args.first() {
                            Some(Val::List(items)) => {
                                Val::Str(items.iter().map(Val::show).collect::<Vec<_>>().join(&s))
                            }
                            _ => return Err("str.join expects List".into()),
                        },
                        "chars" => Val::List(s.chars().map(|c| Val::Str(c.to_string())).collect()),
                        "isdigit" => {
                            Val::Bool(!s.is_empty() && s.chars().all(|c| c.is_ascii_digit()))
                        }
                        "isalpha" => {
                            Val::Bool(!s.is_empty() && s.chars().all(|c| c.is_alphabetic()))
                        }
                        "len" => Val::Num(s.chars().count() as f64),
                        _ => return Err(format!("unknown String method '{name}'")),
                    },
                    Val::List(mut items) => match name.as_str() {
                        "append" | "push" => {
                            if args.len() != 1 {
                                return Err("list.append expects 1 argument".into());
                            }
                            items.push(args.into_iter().next().unwrap());
                            Val::List(items)
                        }
                        "extend" => {
                            if args.len() != 1 {
                                return Err("list.extend expects List".into());
                            }
                            match args.into_iter().next().unwrap() {
                                Val::List(other) => {
                                    items.extend(other);
                                    Val::List(items)
                                }
                                _ => return Err("list.extend expects List".into()),
                            }
                        }
                        "pop" => items.pop().ok_or("pop from empty list")?,
                        "sort" => {
                            items.sort_by(|x, y| match (x, y) {
                                (Val::Num(a), Val::Num(b)) => {
                                    a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)
                                }
                                (Val::Str(a), Val::Str(b)) => a.cmp(b),
                                _ => std::cmp::Ordering::Equal,
                            });
                            Val::List(items)
                        }
                        "reverse" => {
                            items.reverse();
                            Val::List(items)
                        }
                        "clear" => {
                            items.clear();
                            Val::List(items)
                        }
                        "insert" => {
                            if args.len() != 2 {
                                return Err("list.insert expects (index, value)".into());
                            }
                            let idx = match args[0] {
                                Val::Num(n) => (n.max(0.0) as usize).min(items.len()),
                                _ => return Err("list.insert index must be Number".into()),
                            };
                            items.insert(idx, args[1].clone());
                            Val::List(items)
                        }
                        "remove" => {
                            if args.len() != 1 {
                                return Err("list.remove expects 1 argument".into());
                            }
                            if let Some(pos) = items.iter().position(|x| *x == args[0]) {
                                items.remove(pos);
                            }
                            Val::List(items)
                        }
                        "index" => {
                            if args.len() != 1 {
                                return Err("list.index expects 1 argument".into());
                            }
                            items
                                .iter()
                                .position(|x| *x == args[0])
                                .map(|i| Val::Num(i as f64))
                                .ok_or("value is not in list")?
                        }
                        "count" => {
                            if args.len() != 1 {
                                return Err("list.count expects 1 argument".into());
                            }
                            Val::Num(items.iter().filter(|x| **x == args[0]).count() as f64)
                        }
                        "contains" => {
                            if args.len() != 1 {
                                return Err("list.contains expects 1 argument".into());
                            }
                            Val::Bool(items.contains(&args[0]))
                        }
                        "copy" => Val::List(items),
                        "join" => {
                            let sep = match args.first() {
                                Some(Val::Str(s)) => s.as_str(),
                                None => "",
                                _ => return Err("list.join separator must be String".into()),
                            };
                            Val::Str(items.iter().map(Val::show).collect::<Vec<_>>().join(sep))
                        }
                        "len" => Val::Num(items.len() as f64),
                        _ => return Err(format!("unknown List method '{name}'")),
                    },
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
                let v = if let Some(v) = self.call_builtin(&n, a.clone())? {
                    v
                } else if self.classes.contains_key(&n) {
                    self.instantiate(&n, a)?
                } else if let Some(Val::Function(target)) = self.get_var(&n) {
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
                if self.vars.keys().any(|k| !self.cells.contains_key(k)) {
                    let cells_mut = Arc::make_mut(&mut self.cells);
                    for (k, v) in &self.vars {
                        cells_mut
                            .entry(k.clone())
                            .or_insert_with(|| Arc::new(Mutex::new(v.clone())));
                    }
                }
                let upvalues = self.cells.clone();
                Arc::make_mut(&mut self.funcs).insert(
                    name,
                    FunctionDef {
                        params: Arc::new(params),
                        return_type,
                        is_async,
                        code: Arc::new(code),
                        captures: Arc::new(HashMap::new()),
                        capture_types: self.var_types.clone(),
                        upvalues,
                    },
                );
            }
            Op::DefClass(name, parent, methods) => {
                if let Some(ref p) = parent {
                    if !self.classes.contains_key(p) {
                        return Err(format!("unknown parent class '{p}'"));
                    }
                }
                if self.vars.keys().any(|k| !self.cells.contains_key(k)) {
                    let cells_mut = Arc::make_mut(&mut self.cells);
                    for (k, v) in &self.vars {
                        cells_mut
                            .entry(k.clone())
                            .or_insert_with(|| Arc::new(Mutex::new(v.clone())));
                    }
                }
                let upvalues = self.cells.clone();
                let mut compiled = HashMap::new();
                for (method, params, return_type, is_async, code) in methods {
                    compiled.insert(
                        method,
                        FunctionDef {
                            params: Arc::new(params),
                            return_type,
                            is_async,
                            code: Arc::new(code),
                            captures: Arc::new(HashMap::new()),
                            capture_types: self.var_types.clone(),
                            upvalues: upvalues.clone(),
                        },
                    );
                }
                Arc::make_mut(&mut self.classes).insert(
                    name,
                    ClassDef {
                        parent,
                        methods: compiled,
                    },
                );
            }
            Op::Return => {
                self.finished = true;
                return Ok(Some(self.pop()?));
            }
            Op::Yield => return Ok(Some(self.pop()?)),
            Op::PushHandler(target, name) => self.handlers.push((target, name, self.stack.len())),
            Op::PopHandler => {
                let _ = self.handlers.pop();
            }
            Op::Throw => {
                let value = self.pop()?;
                if let Some((target, name, depth)) = self.handlers.pop() {
                    self.stack.truncate(depth);
                    self.set_var(name, value);
                    self.ip = target;
                } else {
                    return Err(format!("uncaught throw: {}", value.show()));
                }
            }
            Op::Halt => {
                self.finished = true;
                return Ok(Some(Val::Null));
            }
        }
        Ok(None)
    }
}
