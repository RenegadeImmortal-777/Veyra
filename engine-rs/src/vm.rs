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

use crate::bytecode::{
    ClassDef, FunctionDef, ObjectData, Op, code_has_yield, collect_referenced_names,
    compute_local_count,
};
use crate::dap::{DAP_TERMINATED_ERR, DebugFrameGuard, DebugSession};
use crate::runtime::{
    GENERATOR_IDS, OBJECT_IDS, TASK_IDS, export_function_object, resolve_function_object, tasks,
};
use crate::tensor::{collect_val_tensor_ids, tensor_gc_unreachable};
use crate::value::Val;

pub(crate) struct CallFrame {
    code: Arc<Vec<Op>>,
    ip: usize,
    vars: HashMap<String, Val>,
    cells: Arc<HashMap<String, Arc<Mutex<Val>>>>,
    var_types: Arc<HashMap<String, String>>,
    consts: HashSet<String>,
    locals: Vec<Val>,
    local_defined: Vec<bool>,
    local_consts: Vec<bool>,
    local_types: Vec<Option<String>>,
    handlers: Vec<(usize, String, usize)>,
    return_type: Option<String>,
    debug_scope: usize,
    dap_pushed: bool,
}

struct PreparedCall {
    code: Arc<Vec<Op>>,
    vars: HashMap<String, Val>,
    cells: Arc<HashMap<String, Arc<Mutex<Val>>>>,
    var_types: Arc<HashMap<String, String>>,
    locals: Vec<Val>,
    local_defined: Vec<bool>,
    local_consts: Vec<bool>,
    local_types: Vec<Option<String>>,
    return_type: Option<String>,
    is_generator: bool,
    debug_name: String,
    debug_scope: usize,
}

pub(crate) struct Vm {
    pub(crate) code: Arc<Vec<Op>>,
    pub(crate) ip: usize,
    pub(crate) stack: Vec<Val>,
    pub(crate) vars: HashMap<String, Val>,
    pub(crate) cells: Arc<HashMap<String, Arc<Mutex<Val>>>>,
    pub(crate) var_types: Arc<HashMap<String, String>>,
    pub(crate) consts: HashSet<String>,
    pub(crate) locals: Vec<Val>,
    pub(crate) local_defined: Vec<bool>,
    pub(crate) local_consts: Vec<bool>,
    pub(crate) local_types: Vec<Option<String>>,
    pub(crate) return_type: Option<String>,
    pub(crate) frames: Vec<CallFrame>,
    pub(crate) funcs: Arc<HashMap<String, FunctionDef>>,
    pub(crate) classes: Arc<HashMap<String, ClassDef>>,
    pub(crate) method_cache: HashMap<(String, String), FunctionDef>,
    pub(crate) objects: Arc<Mutex<HashMap<u64, ObjectData>>>,
    pub(crate) handlers: Vec<(usize, String, usize)>,
    pub(crate) breakpoints: HashSet<usize>,
    pub(crate) trace: bool,
    pub(crate) finished: bool,
    pub(crate) debug_scope: usize,
    pub(crate) debugger: Option<Arc<DebugSession>>,
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
        let local_count = compute_local_count(&code, 0);
        Self {
            code: Arc::new(code),
            ip: 0,
            stack: vec![],
            vars: HashMap::new(),
            cells: Arc::new(HashMap::new()),
            var_types: Arc::new(HashMap::new()),
            consts: HashSet::new(),
            locals: vec![Val::Null; local_count],
            local_defined: vec![false; local_count],
            local_consts: vec![false; local_count],
            local_types: vec![None; local_count],
            return_type: None,
            frames: Vec::new(),
            funcs: Arc::new(HashMap::new()),
            classes: Arc::new(HashMap::new()),
            method_cache: HashMap::new(),
            objects: Arc::new(Mutex::new(HashMap::new())),
            handlers: vec![],
            breakpoints: HashSet::new(),
            trace: false,
            finished: false,
            debug_scope: 0,
            debugger: None,
        }
    }
    fn nested_from_prepared(&self, prepared: PreparedCall) -> Self {
        Self {
            code: prepared.code,
            ip: 0,
            stack: vec![],
            vars: prepared.vars,
            cells: prepared.cells,
            var_types: prepared.var_types,
            consts: HashSet::new(),
            locals: prepared.locals,
            local_defined: prepared.local_defined,
            local_consts: prepared.local_consts,
            local_types: prepared.local_types,
            return_type: None,
            frames: Vec::new(),
            funcs: self.funcs.clone(),
            classes: self.classes.clone(),
            method_cache: HashMap::new(),
            objects: self.objects.clone(),
            handlers: vec![],
            breakpoints: HashSet::new(),
            trace: false,
            finished: false,
            debug_scope: prepared.debug_scope,
            debugger: None,
        }
    }
    fn child_debug_scope(&self, def_ip: usize, sub_idx: usize) -> usize {
        self.debugger
            .as_ref()
            .and_then(|d| d.child_scope(self.debug_scope, def_ip, sub_idx))
            .unwrap_or(0)
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
    pub(crate) fn find_method_cached(
        &mut self,
        class_name: &str,
        method: &str,
    ) -> Option<FunctionDef> {
        let key = (class_name.to_string(), method.to_string());
        if let Some(cached) = self.method_cache.get(&key) {
            return Some(cached.clone());
        }
        let found = self.find_method(class_name, method)?;
        self.method_cache.insert(key, found.clone());
        Some(found)
    }
    fn prepare_call(&self, def: FunctionDef, args: Vec<Val>) -> Result<PreparedCall, String> {
        self.prepare_call_kw(def, args, Vec::new())
    }
    fn prepare_call_kw(
        &self,
        def: FunctionDef,
        mut args: Vec<Val>,
        kwargs: Vec<(String, Val)>,
    ) -> Result<PreparedCall, String> {
        let has_kwargs = def.params.last().and_then(|(_, ty)| ty.as_deref()) == Some("**");
        let non_kw_len = if has_kwargs {
            def.params.len().saturating_sub(1)
        } else {
            def.params.len()
        };
        let has_varargs = non_kw_len > 0 && def.params[non_kw_len - 1].1.as_deref() == Some("*");

        if kwargs.is_empty() && !has_kwargs {
            if has_varargs {
                let fixed = def.params.len().saturating_sub(1);
                if args.len() < fixed {
                    return Err(format!(
                        "function expects at least {fixed} arguments, got {}",
                        args.len()
                    ));
                }
                let rest = args.split_off(fixed);
                args.push(Val::List(rest));
            } else if args.len() < def.params.len() {
                let missing_all_optional = def.params[args.len()..]
                    .iter()
                    .all(|(_, ty)| ty.as_deref() == Some("?default"));
                if missing_all_optional {
                    args.resize(def.params.len(), Val::Null);
                } else {
                    return Err(format!(
                        "function expects {} arguments, got {}",
                        def.params.len(),
                        args.len()
                    ));
                }
            } else if def.params.len() != args.len() {
                return Err(format!(
                    "function expects {} arguments, got {}",
                    def.params.len(),
                    args.len()
                ));
            }
        } else {
            let fixed = non_kw_len.saturating_sub(if has_varargs { 1 } else { 0 });
            let mut slots: Vec<Option<Val>> = vec![None; fixed];
            let mut rest = Vec::new();
            for (idx, val) in args.into_iter().enumerate() {
                if idx < fixed {
                    slots[idx] = Some(val);
                } else if has_varargs {
                    rest.push(val);
                } else {
                    return Err(format!(
                        "function expects at most {fixed} positional arguments"
                    ));
                }
            }
            let mut kw_map: Vec<(Val, Val)> = Vec::new();
            for (k, v) in kwargs {
                if let Some(pos) = def.params[..fixed].iter().position(|(p, _)| p == &k) {
                    if slots[pos].is_some() {
                        return Err(format!("multiple values for argument '{k}'"));
                    }
                    slots[pos] = Some(v);
                } else if has_kwargs {
                    let ks = Val::Str(k);
                    if let Some((_, existing)) = kw_map.iter_mut().find(|(ek, _)| *ek == ks) {
                        *existing = v;
                    } else {
                        kw_map.push((ks, v));
                    }
                } else {
                    return Err(format!("unexpected keyword argument '{k}'"));
                }
            }
            let mut resolved = Vec::with_capacity(def.params.len());
            for (idx, (param, ty)) in def.params[..fixed].iter().enumerate() {
                if let Some(val) = slots[idx].take() {
                    resolved.push(val);
                } else if ty.as_deref() == Some("?default") {
                    resolved.push(Val::Null);
                } else {
                    return Err(format!("missing required argument '{param}'"));
                }
            }
            if has_varargs {
                resolved.push(Val::List(rest));
            }
            if has_kwargs {
                resolved.push(Val::Map(kw_map));
            }
            args = resolved;
        }
        if def.is_async && def.is_generator {
            return Err("async generator functions are not yet supported".into());
        }
        let local_count = def.local_count.max(def.params.len());
        let mut vars = if def.captures.is_empty() {
            HashMap::new()
        } else {
            (*def.captures).clone()
        };
        let mut cells = def.upvalues;
        let mut var_types = def.capture_types;
        let mut local_types = vec![None; local_count];
        for (slot, ((param, ty), value)) in def.params.iter().zip(&args).enumerate() {
            let named = def.param_named.get(slot).copied().unwrap_or(true);
            if let Some(expected) = ty {
                if expected != "*" && expected != "**" && expected != "?default" {
                    if !self.value_matches(value, expected) {
                        return Err(format!(
                            "argument '{param}' expected {expected}, got {}",
                            value.show()
                        ));
                    }
                    local_types[slot] = Some(expected.clone());
                    if named {
                        Arc::make_mut(&mut var_types).insert(param.clone(), expected.clone());
                    }
                }
            }
            if !cells.is_empty() && cells.contains_key(param) {
                Arc::make_mut(&mut cells).remove(param);
            }
            if named {
                vars.insert(param.clone(), value.clone());
            }
        }
        let param_end = args.len().min(local_count);
        let mut locals = args;
        locals.resize(local_count, Val::Null);
        let mut local_defined = vec![false; local_count];
        local_defined[..param_end].fill(true);
        let local_consts = vec![false; local_count];
        Ok(PreparedCall {
            code: def.code,
            vars,
            cells,
            var_types,
            locals,
            local_defined,
            local_consts,
            local_types,
            return_type: def.return_type,
            is_generator: def.is_generator,
            debug_name: def.debug_name,
            debug_scope: def.debug_scope,
        })
    }
    fn push_prepared_frame(&mut self, prepared: PreparedCall) {
        let dap_pushed = if let Some(ref dbg) = self.debugger {
            dbg.update_caller_frame(
                self.debug_scope,
                self.ip.saturating_sub(1),
                &self.vars,
                &self.cells,
                &self.locals,
                &self.local_defined,
            );
            let frame_name = if prepared.debug_name.is_empty() {
                "<fn>".into()
            } else {
                prepared.debug_name
            };
            dbg.push_frame(frame_name, prepared.debug_scope);
            true
        } else {
            false
        };
        let frame = CallFrame {
            code: std::mem::replace(&mut self.code, prepared.code),
            ip: std::mem::replace(&mut self.ip, 0),
            vars: std::mem::replace(&mut self.vars, prepared.vars),
            cells: std::mem::replace(&mut self.cells, prepared.cells),
            var_types: std::mem::replace(&mut self.var_types, prepared.var_types),
            consts: std::mem::take(&mut self.consts),
            locals: std::mem::replace(&mut self.locals, prepared.locals),
            local_defined: std::mem::replace(&mut self.local_defined, prepared.local_defined),
            local_consts: std::mem::replace(&mut self.local_consts, prepared.local_consts),
            local_types: std::mem::replace(&mut self.local_types, prepared.local_types),
            handlers: std::mem::take(&mut self.handlers),
            return_type: std::mem::replace(&mut self.return_type, prepared.return_type),
            debug_scope: std::mem::replace(&mut self.debug_scope, prepared.debug_scope),
            dap_pushed,
        };
        self.frames.push(frame);
    }
    fn restore_frame(&mut self, caller: CallFrame) {
        if caller.dap_pushed {
            if let Some(ref dbg) = self.debugger {
                dbg.pop_frame();
            }
        }
        self.code = caller.code;
        self.ip = caller.ip;
        self.vars = caller.vars;
        self.cells = caller.cells;
        self.var_types = caller.var_types;
        self.consts = caller.consts;
        self.locals = caller.locals;
        self.local_defined = caller.local_defined;
        self.local_consts = caller.local_consts;
        self.local_types = caller.local_types;
        self.handlers = caller.handlers;
        self.return_type = caller.return_type;
        self.debug_scope = caller.debug_scope;
    }
    fn invoke_in_execute(&mut self, def: FunctionDef, args: Vec<Val>) -> Result<(), String> {
        self.invoke_in_execute_kw(def, args, Vec::new())
    }
    fn invoke_in_execute_kw(
        &mut self,
        def: FunctionDef,
        args: Vec<Val>,
        kwargs: Vec<(String, Val)>,
    ) -> Result<(), String> {
        if def.is_async || def.is_generator {
            let v = self.call_def_kw(def, args, kwargs)?;
            self.stack.push(v);
            return Ok(());
        }
        let prepared = self.prepare_call_kw(def, args, kwargs)?;
        self.push_prepared_frame(prepared);
        Ok(())
    }
    fn call_def_kw(
        &self,
        def: FunctionDef,
        args: Vec<Val>,
        kwargs: Vec<(String, Val)>,
    ) -> Result<Val, String> {
        if def.is_async {
            if def.is_generator {
                return Err("async generator functions are not yet supported".into());
            }
            let prepared = self.prepare_call_kw(def, args, kwargs)?;
            return self.spawn_prepared_task(prepared, "async");
        }
        let prepared = self.prepare_call_kw(def, args, kwargs)?;
        self.run_prepared_call(prepared)
    }
    fn spawn_prepared_task(&self, prepared: PreparedCall, kind: &str) -> Result<Val, String> {
        let expected_return = prepared.return_type.clone();
        let mut nested = self.nested_from_prepared(prepared);
        let id = TASK_IDS.fetch_add(1, Ordering::Relaxed);
        let label = kind.to_string();
        let handle = thread::spawn(move || {
            let result = nested.execute()?;
            if let Some(expected) = expected_return {
                if !nested.value_matches(&result, &expected) {
                    return Err(format!(
                        "{label} return value expected {expected}, got {}",
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
        Ok(Val::Num(id as f64))
    }
    pub(crate) fn spawn_task_with_kind(
        &self,
        def: FunctionDef,
        args: Vec<Val>,
        kind: &str,
    ) -> Result<Val, String> {
        let prepared = self.prepare_call(def, args)?;
        self.spawn_prepared_task(prepared, kind)
    }
    pub(crate) fn call_def(&self, def: FunctionDef, args: Vec<Val>) -> Result<Val, String> {
        if def.is_async {
            if def.is_generator {
                return Err("async generator functions are not yet supported".into());
            }
            return self.spawn_task_with_kind(def, args, "async");
        }
        let prepared = self.prepare_call(def, args)?;
        self.run_prepared_call(prepared)
    }
    fn run_prepared_call(&self, prepared: PreparedCall) -> Result<Val, String> {
        let is_generator = prepared.is_generator;
        let return_type = prepared.return_type.clone();
        let debug_name = prepared.debug_name.clone();
        let debug_scope = prepared.debug_scope;
        let mut nested = self.nested_from_prepared(prepared);
        if is_generator {
            if let Some(expected) = return_type {
                if expected != "Generator" && expected != "Any" {
                    return Err(format!(
                        "generator function return type must be Generator or Any, got {expected}"
                    ));
                }
            }
            nested.debugger = self.debugger.clone();
            let id = GENERATOR_IDS.fetch_add(1, Ordering::Relaxed);
            generators()
                .lock()
                .map_err(|_| "generator registry poisoned")?
                .insert(id, nested);
            return Ok(Val::Generator(id));
        }
        let _frame_guard = if let Some(ref dbg) = self.debugger {
            dbg.update_caller_frame(
                self.debug_scope,
                self.ip.saturating_sub(1),
                &self.vars,
                &self.cells,
                &self.locals,
                &self.local_defined,
            );
            nested.debugger = Some(dbg.clone());
            Some(DebugFrameGuard::push(
                dbg.clone(),
                if debug_name.is_empty() {
                    "<fn>".into()
                } else {
                    debug_name
                },
                debug_scope,
            ))
        } else {
            None
        };
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
        self.instantiate_kw(class_name, args, Vec::new())
    }
    pub(crate) fn instantiate_kw(
        &self,
        class_name: &str,
        args: Vec<Val>,
        kwargs: Vec<(String, Val)>,
    ) -> Result<Val, String> {
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
            let _ = self.call_def_kw(init, call_args, kwargs)?;
        } else if !args.is_empty() || !kwargs.is_empty() {
            return Err(format!("{class_name} has no init method"));
        }
        Ok(Val::Object(id))
    }
    pub(crate) fn try_dunder(
        &mut self,
        id: u64,
        method: &str,
        args: Vec<Val>,
    ) -> Result<Option<Val>, String> {
        let class_name = {
            let heap = self.objects.lock().map_err(|_| "object heap poisoned")?;
            heap.get(&id).ok_or("dangling object")?.class_name.clone()
        };
        let Some(def) = self.find_method_cached(&class_name, method) else {
            return Ok(None);
        };
        let mut call_args = vec![Val::Object(id)];
        call_args.extend(args);
        Ok(Some(self.call_def(def, call_args)?))
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
        self.print_val_opts(args, " ", "\n")
    }
    pub(crate) fn print_val_opts(&self, args: &[Val], sep: &str, end: &str) -> Result<Val, String> {
        let rendered = args.iter().map(Val::show).collect::<Vec<_>>().join(sep);
        if let Some(ref dbg) = self.debugger {
            dbg.emit_output(&format!("{rendered}{}", end.trim_end_matches('\n')));
        } else if end == "\n" {
            println!("{rendered}");
        } else {
            use std::io::Write;
            print!("{rendered}{end}");
            let _ = std::io::stdout().flush();
        }
        Ok(Val::Null)
    }
    pub(crate) fn len_val(&mut self, args: &[Val]) -> Result<Val, String> {
        if args.len() != 1 {
            return Err("len expects 1 argument".into());
        }
        Ok(match &args[0] {
            Val::Str(s) => Val::Num(s.chars().count() as f64),
            Val::List(v) => Val::Num(v.len() as f64),
            Val::Map(v) if crate::py_bridge::is_py_handle(&args[0]) => {
                crate::py_bridge::py_call_method(&args[0], "__len__", &[], None)?
            }
            Val::Map(v) => Val::Num(v.len() as f64),
            Val::Object(id) => {
                if let Some(val) = self.try_dunder(*id, "__len__", vec![])? {
                    val
                } else {
                    return Err("len supports String/List/Map or Object with __len__".into());
                }
            }
            _ => return Err("len supports String/List/Map".into()),
        })
    }
    pub(crate) fn show_val_runtime(&mut self, val: &Val) -> Result<String, String> {
        match val {
            Val::Object(id) => {
                if let Some(res) = self.try_dunder(*id, "__str__", vec![])? {
                    return Ok(res.show());
                }
                Ok(val.show())
            }
            Val::Map(_) if crate::py_bridge::is_py_handle(val) => crate::py_bridge::py_str(val),
            _ => Ok(val.show()),
        }
    }
    pub(crate) fn iterable_values(&self, val: &Val) -> Result<Vec<Val>, String> {
        match val {
            Val::List(v) => Ok(v.clone()),
            Val::Str(s) => Ok(s.chars().map(|c| Val::Str(c.to_string())).collect()),
            Val::Map(pairs) => {
                if crate::py_bridge::is_py_handle(val) {
                    let built = crate::py_bridge::py_import("builtins")?;
                    let res = crate::py_bridge::py_call_method(
                        &built,
                        "list",
                        std::slice::from_ref(val),
                        None,
                    )?;
                    return match res {
                        Val::List(v) => Ok(v),
                        _ => Ok(vec![res]),
                    };
                }
                Ok(pairs.iter().map(|(k, _)| k.clone()).collect())
            }
            Val::Generator(id) => collect_generator(*id),
            _ => Err("value is not iterable".into()),
        }
    }
    pub(crate) fn invoke_callable_sync(
        &mut self,
        callee: &Val,
        args: Vec<Val>,
    ) -> Result<Val, String> {
        if crate::py_bridge::is_py_handle(callee) {
            return crate::py_bridge::py_call(callee, &args, None);
        }
        if let Val::Object(id) = callee {
            if let Some(res) = self.try_dunder(*id, "__call__", args.clone())? {
                return Ok(res);
            }
            return Err("object has no __call__ method".into());
        }
        let (_, def) = self.callable_def(callee)?;
        self.call_def(def, args)
    }
    pub(crate) fn sorted_with_opts(
        &mut self,
        mut items: Vec<Val>,
        key_fn: Option<Val>,
        reverse: bool,
    ) -> Result<Vec<Val>, String> {
        let mut pairs: Vec<(Val, usize)> = Vec::with_capacity(items.len());
        if let Some(kfn) = key_fn {
            for (i, item) in items.iter().enumerate() {
                let k = self.invoke_callable_sync(&kfn, vec![item.clone()])?;
                pairs.push((k, i));
            }
        } else {
            for (i, item) in items.iter().enumerate() {
                pairs.push((item.clone(), i));
            }
        }
        pairs.sort_by(|(a, _), (b, _)| {
            let ord = match (a, b) {
                (Val::Num(x), Val::Num(y)) => x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal),
                (Val::Str(x), Val::Str(y)) => x.cmp(y),
                _ => a.show().cmp(&b.show()),
            };
            if reverse { ord.reverse() } else { ord }
        });
        let mut out = Vec::with_capacity(pairs.len());
        for (_, idx) in pairs {
            out.push(std::mem::replace(&mut items[idx], Val::Null));
        }
        Ok(out)
    }
    fn pop_kw(kwargs: &mut Vec<(String, Val)>, key: &str) -> Option<Val> {
        kwargs
            .iter()
            .position(|(k, _)| k == key)
            .map(|idx| kwargs.remove(idx).1)
    }
    pub(crate) fn call_builtin_kw(
        &mut self,
        n: &str,
        pos: Vec<Val>,
        mut kwargs: Vec<(String, Val)>,
    ) -> Result<Option<Val>, String> {
        if kwargs.is_empty() {
            return self.call_builtin(n, pos);
        }
        let out = match n {
            "print" => {
                let sep = match Self::pop_kw(&mut kwargs, "sep") {
                    Some(Val::Str(s)) => s,
                    Some(Val::Null) | None => " ".into(),
                    Some(_) => return Err("print sep must be String".into()),
                };
                let end = match Self::pop_kw(&mut kwargs, "end") {
                    Some(Val::Str(s)) => s,
                    Some(Val::Null) | None => "\n".into(),
                    Some(_) => return Err("print end must be String".into()),
                };
                if let Some((bad, _)) = kwargs.first() {
                    return Err(format!("print got unexpected keyword argument '{bad}'"));
                }
                self.print_val_opts(&pos, &sep, &end)?
            }
            "sorted" | "sort" => {
                if pos.len() != 1 {
                    return Err("sorted expects iterable as first positional argument".into());
                }
                let key_fn = Self::pop_kw(&mut kwargs, "key").filter(|v| !matches!(v, Val::Null));
                let reverse = match Self::pop_kw(&mut kwargs, "reverse") {
                    Some(v) => v.truthy(),
                    None => false,
                };
                if let Some((bad, _)) = kwargs.first() {
                    return Err(format!("sorted got unexpected keyword argument '{bad}'"));
                }
                let items = self.iterable_values(&pos[0])?;
                Val::List(self.sorted_with_opts(items, key_fn, reverse)?)
            }
            "enumerate" => {
                if pos.len() != 1 {
                    return Err("enumerate expects iterable as first positional argument".into());
                }
                let start = match Self::pop_kw(&mut kwargs, "start") {
                    Some(Val::Num(s)) => s as i64,
                    None => 0,
                    _ => return Err("enumerate start must be Number".into()),
                };
                if let Some((bad, _)) = kwargs.first() {
                    return Err(format!("enumerate got unexpected keyword argument '{bad}'"));
                }
                let items = self.iterable_values(&pos[0])?;
                Val::List(
                    items
                        .into_iter()
                        .enumerate()
                        .map(|(i, v)| Val::List(vec![Val::Num((start + i as i64) as f64), v]))
                        .collect(),
                )
            }
            "round" => {
                if pos.len() != 1 {
                    return Err("round expects number as first positional argument".into());
                }
                let ndigits = match Self::pop_kw(&mut kwargs, "ndigits") {
                    Some(Val::Num(d)) => d as i32,
                    Some(Val::Null) | None => 0,
                    _ => return Err("round ndigits must be Number".into()),
                };
                if let Some((bad, _)) = kwargs.first() {
                    return Err(format!("round got unexpected keyword argument '{bad}'"));
                }
                let Val::Num(x) = pos[0] else {
                    return Err("round expects Number".into());
                };
                if ndigits <= 0 {
                    let scale = 10f64.powi(-ndigits);
                    Val::Num((x / scale).round() * scale)
                } else {
                    let scale = 10f64.powi(ndigits);
                    Val::Num((x * scale).round() / scale)
                }
            }
            "pow" => {
                if pos.len() != 2 {
                    return Err("pow expects base, exp".into());
                }
                let mod_v = Self::pop_kw(&mut kwargs, "mod").filter(|v| !matches!(v, Val::Null));
                if let Some((bad, _)) = kwargs.first() {
                    return Err(format!("pow got unexpected keyword argument '{bad}'"));
                }
                let (Val::Num(b), Val::Num(e)) = (&pos[0], &pos[1]) else {
                    return Err("pow expects Numbers".into());
                };
                if let Some(Val::Num(m)) = mod_v {
                    if m == 0.0 {
                        return Err("pow modulus cannot be zero".into());
                    }
                    Val::Num(b.powf(*e) % m)
                } else {
                    Val::Num(b.powf(*e))
                }
            }
            "min" | "max" => {
                if pos.is_empty() {
                    return Err(format!("{n} expects iterable or items"));
                }
                let key_fn = Self::pop_kw(&mut kwargs, "key").filter(|v| !matches!(v, Val::Null));
                if let Some((bad, _)) = kwargs.first() {
                    return Err(format!("{n} got unexpected keyword argument '{bad}'"));
                }
                let items = if pos.len() == 1 {
                    self.iterable_values(&pos[0])?
                } else {
                    pos
                };
                if items.is_empty() {
                    return Err(format!("{n} arg is an empty sequence"));
                }
                let sorted = self.sorted_with_opts(items, key_fn, n == "max")?;
                sorted.into_iter().next().unwrap()
            }
            "py_call" => {
                if pos.is_empty() {
                    return Err("py_call expects callable".into());
                }
                let fn_val = &pos[0];
                let call_args = match pos.get(1) {
                    Some(Val::List(xs)) if pos.len() == 2 && !kwargs.is_empty() => xs.clone(),
                    _ => pos[1..].to_vec(),
                };
                let kw_map = Val::Map(kwargs.into_iter().map(|(k, v)| (Val::Str(k), v)).collect());
                crate::py_bridge::py_call(fn_val, &call_args, Some(&kw_map))?
            }
            _ => return Ok(None),
        };
        Ok(Some(out))
    }
    pub(crate) fn repr_val_runtime(&mut self, val: &Val) -> Result<String, String> {
        match val {
            Val::Str(s) => Ok(format!("{s:?}")),
            Val::Object(id) => {
                if let Some(res) = self.try_dunder(*id, "__repr__", vec![])? {
                    return Ok(res.show());
                }
                Ok(val.show())
            }
            Val::Map(_) if crate::py_bridge::is_py_handle(val) => crate::py_bridge::py_repr(val),
            Val::List(xs) => {
                let mut parts = Vec::with_capacity(xs.len());
                for x in xs {
                    parts.push(self.repr_val_runtime(x)?);
                }
                Ok(format!("[{}]", parts.join(", ")))
            }
            _ => Ok(val.show()),
        }
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
        for v in &self.locals {
            collect_val_tensor_ids(v, &mut live);
        }
        for cell in self.cells.values() {
            if let Ok(guard) = cell.lock() {
                collect_val_tensor_ids(&guard, &mut live);
            }
        }
        for frame in &self.frames {
            for v in frame.vars.values() {
                collect_val_tensor_ids(v, &mut live);
            }
            for v in &frame.locals {
                collect_val_tensor_ids(v, &mut live);
            }
            for cell in frame.cells.values() {
                if let Ok(guard) = cell.lock() {
                    collect_val_tensor_ids(&guard, &mut live);
                }
            }
        }
        for v in &self.stack {
            collect_val_tensor_ids(v, &mut live);
        }
        tensor_gc_unreachable(&live)
    }
    fn eval_cmp(cmp: &Op, x: f64, y: f64) -> bool {
        match cmp {
            Op::Lt => x < y,
            Op::Le => x <= y,
            Op::Gt => x > y,
            _ => x >= y,
        }
    }
    fn apply_arith_mut(op: &Op, target: &mut f64, rhs: f64) {
        match op {
            Op::Add => *target += rhs,
            Op::Sub => *target -= rhs,
            _ => *target *= rhs,
        }
    }
    fn run_accum_loop(i_val: &mut f64, s_val: &mut f64, limit: f64, step: f64, is_add: bool) {
        while *i_val < limit {
            if is_add {
                *s_val += *i_val;
            } else {
                *s_val -= *i_val;
            }
            *i_val += step;
        }
    }
    fn val_index_list(val: &Val, i: usize) -> Option<Result<Val, String>> {
        match val {
            Val::List(v) => Some(
                v.get(i)
                    .cloned()
                    .ok_or_else(|| format!("list index {i} out of range")),
            ),
            _ => None,
        }
    }
    fn val_len(val: &Val) -> Option<usize> {
        match val {
            Val::List(v) => Some(v.len()),
            Val::Str(s) => Some(s.chars().count()),
            Val::Map(m) => Some(m.len()),
            _ => None,
        }
    }
    fn capture_upvalues(&mut self, names: &HashSet<String>) -> HashMap<String, Arc<Mutex<Val>>> {
        let mut captured = HashMap::new();
        for k in names {
            if let Some(existing_cell) = self.cells.get(k) {
                captured.insert(k.clone(), existing_cell.clone());
            } else if let Some(val) = self.vars.get(k) {
                let cell = Arc::new(Mutex::new(val.clone()));
                Arc::make_mut(&mut self.cells).insert(k.clone(), cell.clone());
                captured.insert(k.clone(), cell);
            }
        }
        captured
    }
    #[allow(clippy::too_many_arguments)]
    fn build_function_def(
        &self,
        params: Vec<(String, Option<String>)>,
        return_type: Option<String>,
        is_async: bool,
        code: Vec<Op>,
        debug_name: String,
        debug_scope: usize,
        upvalues: Arc<HashMap<String, Arc<Mutex<Val>>>>,
        param_named: Vec<bool>,
    ) -> FunctionDef {
        let local_count = compute_local_count(&code, params.len());
        let is_generator = code_has_yield(&code);
        FunctionDef {
            params: Arc::new(params),
            return_type,
            is_async,
            is_generator,
            code: Arc::new(code),
            captures: Arc::new(HashMap::new()),
            capture_types: self.var_types.clone(),
            upvalues,
            local_count,
            param_named: Arc::new(param_named),
            debug_name,
            debug_scope,
        }
    }
    fn pop_frame_or_finish(&mut self, val: Val, context: &str) -> Result<Option<Val>, String> {
        let expected_ret = self.return_type.clone();
        if let Some(caller) = self.frames.pop() {
            self.restore_frame(caller);
            if let Some(expected) = expected_ret {
                if !self.value_matches(&val, &expected) {
                    return Err(format!(
                        "{context} value expected {expected}, got {}",
                        val.show()
                    ));
                }
            }
            self.stack.push(val);
            Ok(None)
        } else {
            if let Some(expected) = expected_ret {
                if !self.value_matches(&val, &expected) {
                    return Err(format!(
                        "{context} value expected {expected}, got {}",
                        val.show()
                    ));
                }
            }
            self.finished = true;
            Ok(Some(val))
        }
    }
    pub(crate) fn execute(&mut self) -> Result<Val, String> {
        let _root_guard = match &self.debugger {
            Some(dbg) if self.debug_scope == 0 && dbg.frame_depth() == 0 => {
                Some(DebugFrameGuard::push(dbg.clone(), "<main>".into(), 0))
            }
            _ => None,
        };
        loop {
            if !self.trace && !self.breakpoints.contains(&self.ip) && self.debugger.is_none() {
                if let Some(loop_win) = self.code.get(self.ip..self.ip + 13) {
                    match loop_win {
                        [
                            Op::LoadLocal(i1),
                            limit_op @ (Op::LoadLocal(_) | Op::Load(_) | Op::Const(Val::Num(_))),
                            Op::Lt,
                            Op::JumpIfFalse(target),
                            Op::LoadLocal(s1),
                            Op::LoadLocal(i2),
                            acc_op @ (Op::Add | Op::Sub),
                            Op::StoreLocal(s2),
                            Op::LoadLocal(i3),
                            Op::Const(Val::Num(step)),
                            Op::Add,
                            Op::StoreLocal(i4),
                            Op::Jump(back),
                        ] if *back == self.ip
                            && *target == self.ip + 13
                            && *step > 0.0
                            && i1 == i2
                            && i1 == i3
                            && i1 == i4
                            && s1 == s2
                            && i1 != s1
                            && self.local_defined.get(*i1).copied().unwrap_or(false)
                            && self.local_defined.get(*s1).copied().unwrap_or(false)
                            && !self.local_consts.get(*i1).copied().unwrap_or(true)
                            && !self.local_consts.get(*s1).copied().unwrap_or(true)
                            && self.local_types.get(*i1).and_then(Option::as_ref).is_none()
                            && self.local_types.get(*s1).and_then(Option::as_ref).is_none() =>
                        {
                            let limit_opt = match limit_op {
                                Op::LoadLocal(lim_slot)
                                    if lim_slot != i1
                                        && lim_slot != s1
                                        && self
                                            .local_defined
                                            .get(*lim_slot)
                                            .copied()
                                            .unwrap_or(false) =>
                                {
                                    match self.locals.get(*lim_slot) {
                                        Some(Val::Num(n)) => Some(*n),
                                        _ => None,
                                    }
                                }
                                Op::Load(lim_name) if !self.cells.contains_key(lim_name) => {
                                    match self.vars.get(lim_name) {
                                        Some(Val::Num(n)) => Some(*n),
                                        _ => None,
                                    }
                                }
                                Op::Const(Val::Num(n)) => Some(*n),
                                _ => None,
                            };
                            if let (
                                Some(limit),
                                Some(Val::Num(mut i_val)),
                                Some(Val::Num(mut s_val)),
                            ) = (
                                limit_opt,
                                self.locals.get(*i1).cloned(),
                                self.locals.get(*s1).cloned(),
                            ) {
                                let is_add = matches!(acc_op, Op::Add);
                                Self::run_accum_loop(&mut i_val, &mut s_val, limit, *step, is_add);
                                self.locals[*i1] = Val::Num(i_val);
                                self.locals[*s1] = Val::Num(s_val);
                                self.ip = *target;
                                continue;
                            }
                        }
                        [
                            Op::Load(i1),
                            limit_op @ (Op::Load(_) | Op::Const(Val::Num(_))),
                            Op::Lt,
                            Op::JumpIfFalse(target),
                            Op::Load(s1),
                            Op::Load(i2),
                            acc_op @ (Op::Add | Op::Sub),
                            Op::Store(s2),
                            Op::Load(i3),
                            Op::Const(Val::Num(step)),
                            Op::Add,
                            Op::Store(i4),
                            Op::Jump(back),
                        ] if *back == self.ip
                            && *target == self.ip + 13
                            && *step > 0.0
                            && i1 == i2
                            && i1 == i3
                            && i1 == i4
                            && s1 == s2
                            && i1 != s1
                            && !self.cells.contains_key(i1)
                            && !self.cells.contains_key(s1)
                            && !self.consts.contains(i1)
                            && !self.consts.contains(s1)
                            && !self.var_types.contains_key(i1)
                            && !self.var_types.contains_key(s1) =>
                        {
                            let limit_opt = match limit_op {
                                Op::Load(lim_name)
                                    if lim_name != i1
                                        && lim_name != s1
                                        && !self.cells.contains_key(lim_name) =>
                                {
                                    match self.vars.get(lim_name) {
                                        Some(Val::Num(n)) => Some(*n),
                                        _ => None,
                                    }
                                }
                                Op::Const(Val::Num(n)) => Some(*n),
                                _ => None,
                            };
                            if let (
                                Some(limit),
                                Some(Val::Num(mut i_val)),
                                Some(Val::Num(mut s_val)),
                            ) = (
                                limit_opt,
                                self.vars.get(i1).cloned(),
                                self.vars.get(s1).cloned(),
                            ) {
                                let is_add = matches!(acc_op, Op::Add);
                                let st = *step;
                                let iv_name = i1.clone();
                                let sv_name = s1.clone();
                                let exit_ip = *target;
                                Self::run_accum_loop(&mut i_val, &mut s_val, limit, st, is_add);
                                self.vars.insert(iv_name, Val::Num(i_val));
                                self.vars.insert(sv_name, Val::Num(s_val));
                                self.ip = exit_ip;
                                continue;
                            }
                        }
                        _ => {}
                    }
                }
                if let Some(window) = self.code.get(self.ip..self.ip + 4) {
                    match window {
                        [
                            Op::LoadLocal(a),
                            Op::LoadLocal(b),
                            cmp @ (Op::Lt | Op::Le | Op::Gt | Op::Ge),
                            Op::JumpIfFalse(target),
                        ] if self.local_defined.get(*a).copied().unwrap_or(false)
                            && self.local_defined.get(*b).copied().unwrap_or(false) =>
                        {
                            if let (Some(Val::Num(x)), Some(Val::Num(y))) =
                                (self.locals.get(*a), self.locals.get(*b))
                            {
                                let yes = Self::eval_cmp(cmp, *x, *y);
                                self.ip = if yes { self.ip + 4 } else { *target };
                                continue;
                            }
                        }
                        [
                            Op::LoadLocal(a),
                            Op::Const(Val::Num(y)),
                            cmp @ (Op::Lt | Op::Le | Op::Gt | Op::Ge),
                            Op::JumpIfFalse(target),
                        ] if self.local_defined.get(*a).copied().unwrap_or(false) => {
                            if let Some(Val::Num(x)) = self.locals.get(*a) {
                                let yes = Self::eval_cmp(cmp, *x, *y);
                                self.ip = if yes { self.ip + 4 } else { *target };
                                continue;
                            }
                        }
                        [
                            Op::LoadLocal(a),
                            Op::Const(Val::Num(k)),
                            op @ (Op::Add | Op::Sub | Op::Mul),
                            Op::StoreLocal(s),
                        ] if a == s
                            && self.local_defined.get(*a).copied().unwrap_or(false)
                            && !self.local_consts.get(*a).copied().unwrap_or(true)
                            && self.local_types.get(*a).and_then(Option::as_ref).is_none() =>
                        {
                            let rhs = *k;
                            let arith = op.clone();
                            if let Some(Val::Num(x)) = self.locals.get_mut(*a) {
                                Self::apply_arith_mut(&arith, x, rhs);
                                self.ip += 4;
                                continue;
                            }
                        }
                        [
                            Op::LoadLocal(a),
                            Op::LoadLocal(b),
                            op @ (Op::Add | Op::Sub | Op::Mul),
                            Op::StoreLocal(s),
                        ] if a == s
                            && self.local_defined.get(*a).copied().unwrap_or(false)
                            && self.local_defined.get(*b).copied().unwrap_or(false)
                            && !self.local_consts.get(*a).copied().unwrap_or(true)
                            && self.local_types.get(*a).and_then(Option::as_ref).is_none() =>
                        {
                            if let Some(Val::Num(y)) = self.locals.get(*b).cloned() {
                                let arith = op.clone();
                                if let Some(Val::Num(x)) = self.locals.get_mut(*a) {
                                    Self::apply_arith_mut(&arith, x, y);
                                    self.ip += 4;
                                    continue;
                                }
                            }
                        }
                        [
                            Op::Load(a),
                            Op::Load(b),
                            cmp @ (Op::Lt | Op::Le | Op::Gt | Op::Ge),
                            Op::JumpIfFalse(target),
                        ] if !self.cells.contains_key(a) && !self.cells.contains_key(b) => {
                            if let (Some(Val::Num(x)), Some(Val::Num(y))) =
                                (self.vars.get(a), self.vars.get(b))
                            {
                                let yes = Self::eval_cmp(cmp, *x, *y);
                                self.ip = if yes { self.ip + 4 } else { *target };
                                continue;
                            }
                        }
                        [
                            Op::Load(a),
                            Op::Const(Val::Num(y)),
                            cmp @ (Op::Lt | Op::Le | Op::Gt | Op::Ge),
                            Op::JumpIfFalse(target),
                        ] if !self.cells.contains_key(a) => {
                            if let Some(Val::Num(x)) = self.vars.get(a) {
                                let yes = Self::eval_cmp(cmp, *x, *y);
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
                            && !self.cells.contains_key(a)
                            && !self.consts.contains(a)
                            && !self.var_types.contains_key(a) =>
                        {
                            let rhs = *k;
                            let arith = op.clone();
                            if let Some(Val::Num(x)) = self.vars.get_mut(a) {
                                Self::apply_arith_mut(&arith, x, rhs);
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
                            && !self.cells.contains_key(a)
                            && !self.cells.contains_key(b)
                            && !self.consts.contains(a)
                            && !self.var_types.contains_key(a) =>
                        {
                            if let Some(Val::Num(y)) = self.vars.get(b).cloned() {
                                let arith = op.clone();
                                if let Some(Val::Num(x)) = self.vars.get_mut(a) {
                                    Self::apply_arith_mut(&arith, x, y);
                                    self.ip += 4;
                                    continue;
                                }
                            }
                        }
                        _ => {}
                    }
                }
                match self.code.get(self.ip) {
                    Some(Op::Jump(target)) => {
                        self.ip = *target;
                        continue;
                    }
                    Some(Op::LoadLocal(slot))
                        if *slot < self.locals.len()
                            && self.local_defined[*slot]
                            && !matches!(
                                (self.code.get(self.ip + 1), self.code.get(self.ip + 2)),
                                (Some(Op::LoadLocal(_) | Op::Load(_)), Some(Op::Index))
                            )
                            && !matches!(
                                self.code.get(self.ip + 1),
                                Some(Op::Call(fn_name, 1)) if fn_name == "len"
                            ) =>
                    {
                        self.stack.push(self.locals[*slot].clone());
                        self.ip += 1;
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
                for (slot, val) in self.locals.iter().enumerate() {
                    if self.local_defined.get(slot).copied().unwrap_or(false) {
                        vars.push(format!("#{slot}={}", val.show()));
                    }
                }
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
            if let Some(ref dbg) = self.debugger {
                dbg.before_op(
                    self.debug_scope,
                    self.ip,
                    &self.vars,
                    &self.cells,
                    &self.locals,
                    &self.local_defined,
                    &self.funcs,
                    &self.classes,
                    &self.objects,
                )?;
            }
            self.ip += 1;
            match self.step_op(op) {
                Ok(Some(v)) => return Ok(v),
                Ok(None) => {}
                Err(err) => {
                    if err != DAP_TERMINATED_ERR {
                        let mut caught = false;
                        loop {
                            if let Some((target, name, depth)) = self.handlers.pop() {
                                self.stack.truncate(depth);
                                let msg = err
                                    .strip_prefix("uncaught throw: ")
                                    .unwrap_or(&err)
                                    .to_string();
                                self.set_var(name, Val::Str(msg));
                                self.ip = target;
                                caught = true;
                                break;
                            }
                            if let Some(caller) = self.frames.pop() {
                                self.restore_frame(caller);
                            } else {
                                break;
                            }
                        }
                        if caught {
                            continue;
                        }
                    }
                    while let Some(caller) = self.frames.pop() {
                        if caller.dap_pushed {
                            if let Some(ref dbg) = self.debugger {
                                dbg.pop_frame();
                            }
                        }
                    }
                    return Err(err);
                }
            }
        }
    }
    fn step_op(&mut self, op: Op) -> Result<Option<Val>, String> {
        match op {
            Op::Const(v) => self.stack.push(v),
            Op::LoadLocal(slot) => {
                if slot >= self.locals.len() || !self.local_defined[slot] {
                    return Err(format!("undefined local slot {slot}"));
                }
                if let Some(Op::Index) = self.code.get(self.ip + 1) {
                    let idx_num_opt = match self.code.get(self.ip) {
                        Some(Op::LoadLocal(idx_slot))
                            if *idx_slot < self.locals.len() && self.local_defined[*idx_slot] =>
                        {
                            match self.locals[*idx_slot] {
                                Val::Num(n) => Some(n),
                                _ => None,
                            }
                        }
                        Some(Op::Load(idx_name)) => match self.get_var(idx_name) {
                            Some(Val::Num(n)) => Some(n),
                            _ => None,
                        },
                        _ => None,
                    };
                    if let Some(idx_num) = idx_num_opt {
                        if idx_num >= 0.0 {
                            if let Some(res) =
                                Self::val_index_list(&self.locals[slot], idx_num as usize)
                            {
                                self.ip += 2;
                                self.stack.push(res?);
                                return Ok(None);
                            }
                        }
                    }
                }
                if let Some(Op::Call(fn_name, 1)) = self.code.get(self.ip) {
                    if fn_name == "len" {
                        if let Some(len) = Self::val_len(&self.locals[slot]) {
                            self.ip += 1;
                            self.stack.push(Val::Num(len as f64));
                            return Ok(None);
                        }
                    }
                }
                self.stack.push(self.locals[slot].clone());
            }
            Op::DefineLocal(slot, is_const, ty) => {
                let v = self.pop()?;
                if slot >= self.locals.len() {
                    self.locals.resize(slot + 1, Val::Null);
                    self.local_defined.resize(slot + 1, false);
                    self.local_consts.resize(slot + 1, false);
                    self.local_types.resize(slot + 1, None);
                }
                if self.local_defined[slot] && self.local_consts[slot] {
                    return Err("cannot redefine constant local".into());
                }
                if self.local_defined[slot] && is_const {
                    return Err("local already defined".into());
                }
                if let Some(ref expected) = ty {
                    if !self.value_matches(&v, expected) {
                        return Err(format!("declaration expected {expected}, got {}", v.show()));
                    }
                    self.local_types[slot] = Some(expected.clone());
                }
                self.locals[slot] = v;
                self.local_defined[slot] = true;
                if is_const {
                    self.local_consts[slot] = true;
                }
            }
            Op::StoreLocal(slot) => {
                if slot < self.local_consts.len() && self.local_consts[slot] {
                    return Err("cannot assign to constant local".into());
                }
                let v = self.pop()?;
                if slot >= self.locals.len() {
                    self.locals.resize(slot + 1, Val::Null);
                    self.local_defined.resize(slot + 1, false);
                    self.local_consts.resize(slot + 1, false);
                    self.local_types.resize(slot + 1, None);
                }
                if let Some(ref expected) = self.local_types[slot] {
                    if !self.value_matches(&v, expected) {
                        return Err(format!("assignment expected {expected}, got {}", v.show()));
                    }
                }
                self.locals[slot] = v;
                self.local_defined[slot] = true;
            }
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
                                Self::val_index_list(&guard, i).transpose()?
                            } else if let Some(val) = self.vars.get(&n) {
                                Self::val_index_list(val, i).transpose()?
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
                            Self::val_len(&guard)
                        } else {
                            self.vars.get(&n).and_then(Self::val_len)
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
                if let Val::Object(id) = &a {
                    if let Some(res) = self.try_dunder(*id, "__add__", vec![b.clone()])? {
                        self.stack.push(res);
                        return Ok(None);
                    }
                }
                self.stack.push(match (a, b) {
                    (Val::Num(x), Val::Num(y)) => Val::Num(x + y),
                    (Val::Str(x), Val::Str(y)) => Val::Str(x + &y),
                    (Val::List(mut x), Val::List(y)) => {
                        x.extend(y);
                        Val::List(x)
                    }
                    _ => return Err("invalid '+' operands".into()),
                });
            }
            Op::Sub => {
                let b = self.pop()?;
                let a = self.pop()?;
                if let Val::Object(id) = &a {
                    if let Some(res) = self.try_dunder(*id, "__sub__", vec![b.clone()])? {
                        self.stack.push(res);
                        return Ok(None);
                    }
                }
                match (a, b) {
                    (Val::Num(x), Val::Num(y)) => self.stack.push(Val::Num(x - y)),
                    _ => return Err("numeric operation requires numbers".into()),
                }
            }
            Op::Mul => {
                let b = self.pop()?;
                let a = self.pop()?;
                if let Val::Object(id) = &a {
                    if let Some(res) = self.try_dunder(*id, "__mul__", vec![b.clone()])? {
                        self.stack.push(res);
                        return Ok(None);
                    }
                }
                self.stack.push(match (a, b) {
                    (Val::Num(x), Val::Num(y)) => Val::Num(x * y),
                    (Val::Str(s), Val::Num(n)) | (Val::Num(n), Val::Str(s)) => {
                        Val::Str(s.repeat(n.max(0.0) as usize))
                    }
                    (Val::List(items), Val::Num(n)) | (Val::Num(n), Val::List(items)) => {
                        let count = n.max(0.0) as usize;
                        let mut out = Vec::with_capacity(items.len() * count);
                        for _ in 0..count {
                            out.extend(items.iter().cloned());
                        }
                        Val::List(out)
                    }
                    _ => return Err("numeric operation requires numbers".into()),
                });
            }
            Op::Div => self.bin_num(|a, b| a / b)?,
            Op::Mod => self.bin_num(|a, b| a % b)?,
            Op::Pow => self.bin_num(|a, b| a.powf(b))?,
            Op::Eq => {
                let b = self.pop()?;
                let a = self.pop()?;
                if let Val::Object(id) = &a {
                    if let Some(res) = self.try_dunder(*id, "__eq__", vec![b.clone()])? {
                        self.stack.push(Val::Bool(res.truthy()));
                        return Ok(None);
                    }
                }
                self.stack.push(Val::Bool(a == b));
            }
            Op::Ne => {
                let b = self.pop()?;
                let a = self.pop()?;
                if let Val::Object(id) = &a {
                    if let Some(res) = self.try_dunder(*id, "__eq__", vec![b.clone()])? {
                        self.stack.push(Val::Bool(!res.truthy()));
                        return Ok(None);
                    }
                }
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
                if crate::py_bridge::is_py_handle(&obj) {
                    let v = crate::py_bridge::py_getitem(&obj, &key)?;
                    self.stack.push(v);
                    return Ok(None);
                }
                if let Val::Object(id) = &obj {
                    if let Some(v) = self.try_dunder(*id, "__getitem__", vec![key.clone()])? {
                        self.stack.push(v);
                        return Ok(None);
                    }
                }
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
                if crate::py_bridge::is_py_handle(&obj) {
                    let _ = crate::py_bridge::py_setitem(&obj, &key, &value)?;
                    self.stack.push(obj);
                    return Ok(None);
                }
                if let Val::Object(id) = &obj {
                    if self
                        .try_dunder(*id, "__setitem__", vec![key.clone(), value.clone()])?
                        .is_some()
                    {
                        self.stack.push(obj);
                        return Ok(None);
                    }
                }
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
                if crate::py_bridge::is_py_handle(&obj) {
                    let v = crate::py_bridge::py_get(&obj, &name)?;
                    self.stack.push(v);
                    return Ok(None);
                }
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
                if crate::py_bridge::is_py_handle(&obj) {
                    let _ = crate::py_bridge::py_set(&obj, &name, &value)?;
                    return Ok(None);
                }
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
                    Val::Map(_) if crate::py_bridge::is_py_handle(&receiver) => {
                        crate::py_bridge::py_call_method(&receiver, &name, &args, None)?
                    }
                    Val::Object(id) => {
                        let class_name = {
                            let heap = self.objects.lock().map_err(|_| "object heap poisoned")?;
                            heap.get(&id).ok_or("dangling object")?.class_name.clone()
                        };
                        let method = self
                            .find_method_cached(&class_name, &name)
                            .ok_or(format!("undefined method '{name}' on {class_name}"))?;
                        let mut call_args = vec![Val::Object(id)];
                        call_args.extend(args);
                        self.invoke_in_execute(method, call_args)?;
                        return Ok(None);
                    }
                    Val::Map(mut pairs) => {
                        if let Some((_, member)) =
                            pairs.iter().find(|(k, _)| *k == Val::Str(name.clone()))
                        {
                            if let Ok((_, def)) = self.callable_def(member) {
                                self.invoke_in_execute(def, args)?;
                                return Ok(None);
                            }
                            if crate::py_bridge::is_py_handle(member) {
                                let res = crate::py_bridge::py_call(member, &args, None)?;
                                self.stack.push(res);
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
                            "pop" => {
                                if args.is_empty() || args.len() > 2 {
                                    return Err("map.pop expects (key[, default])".into());
                                }
                                if let Some(pos) = pairs.iter().position(|(k, _)| *k == args[0]) {
                                    pairs.remove(pos).1
                                } else if args.len() == 2 {
                                    args[1].clone()
                                } else {
                                    return Err("map.pop key not found".into());
                                }
                            }
                            "update" => {
                                if args.len() != 1 {
                                    return Err("map.update expects Map".into());
                                }
                                if let Val::Map(other) = &args[0] {
                                    for (k, v) in other {
                                        if let Some((_, ex)) =
                                            pairs.iter_mut().find(|(ek, _)| ek == k)
                                        {
                                            *ex = v.clone();
                                        } else {
                                            pairs.push((k.clone(), v.clone()));
                                        }
                                    }
                                    Val::Map(pairs)
                                } else {
                                    return Err("map.update expects Map".into());
                                }
                            }
                            "setdefault" => {
                                if args.is_empty() || args.len() > 2 {
                                    return Err("map.setdefault expects (key[, default])".into());
                                }
                                if let Some((_, existing)) =
                                    pairs.iter().find(|(k, _)| *k == args[0])
                                {
                                    existing.clone()
                                } else {
                                    args.get(1).cloned().unwrap_or(Val::Null)
                                }
                            }
                            "clear" => Val::Map(Vec::new()),
                            "copy" => Val::Map(pairs),
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
                        "rfind" => match args.first() {
                            Some(Val::Str(sub)) => {
                                Val::Num(s.rfind(sub).map(|i| i as f64).unwrap_or(-1.0))
                            }
                            _ => return Err("str.rfind expects String".into()),
                        },
                        "count" => match args.first() {
                            Some(Val::Str(sub)) => Val::Num(s.matches(sub).count() as f64),
                            _ => return Err("str.count expects String".into()),
                        },
                        "join" => match args.first() {
                            Some(val) => {
                                let items = self.iterable_values(val)?;
                                Val::Str(items.iter().map(Val::show).collect::<Vec<_>>().join(&s))
                            }
                            None => return Err("str.join expects iterable".into()),
                        },
                        "chars" => Val::List(s.chars().map(|c| Val::Str(c.to_string())).collect()),
                        "splitlines" => {
                            Val::List(s.lines().map(|l| Val::Str(l.to_string())).collect())
                        }
                        "capitalize" => {
                            let mut chars = s.chars();
                            match chars.next() {
                                None => Val::Str(String::new()),
                                Some(first) => {
                                    let head: String = first.to_uppercase().collect();
                                    let tail = chars.as_str().to_lowercase();
                                    Val::Str(format!("{head}{tail}"))
                                }
                            }
                        }
                        "title" => {
                            let mut out = String::with_capacity(s.len());
                            let mut cap_next = true;
                            for c in s.chars() {
                                if c.is_alphabetic() {
                                    if cap_next {
                                        out.extend(c.to_uppercase());
                                        cap_next = false;
                                    } else {
                                        out.extend(c.to_lowercase());
                                    }
                                } else {
                                    out.push(c);
                                    cap_next = true;
                                }
                            }
                            Val::Str(out)
                        }
                        "zfill" => {
                            let width = match args.first() {
                                Some(Val::Num(n)) => (*n).max(0.0) as usize,
                                _ => return Err("str.zfill expects width Number".into()),
                            };
                            let char_len = s.chars().count();
                            if char_len >= width {
                                Val::Str(s)
                            } else if let Some(rest) = s.strip_prefix(['+', '-']) {
                                let sign = &s[..1];
                                Val::Str(format!("{sign}{}{rest}", "0".repeat(width - char_len)))
                            } else {
                                Val::Str(format!("{}{s}", "0".repeat(width - char_len)))
                            }
                        }
                        "isdigit" => {
                            Val::Bool(!s.is_empty() && s.chars().all(|c| c.is_ascii_digit()))
                        }
                        "isalpha" => {
                            Val::Bool(!s.is_empty() && s.chars().all(|c| c.is_alphabetic()))
                        }
                        "isalnum" => {
                            Val::Bool(!s.is_empty() && s.chars().all(|c| c.is_alphanumeric()))
                        }
                        "isspace" => {
                            Val::Bool(!s.is_empty() && s.chars().all(|c| c.is_whitespace()))
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
                let mut a = Vec::with_capacity(k);
                for _ in 0..k {
                    a.push(self.pop()?)
                }
                a.reverse();
                if let Some(v) = self.call_builtin(&n, a.clone())? {
                    self.stack.push(v);
                } else if self.classes.contains_key(&n) {
                    let v = self.instantiate(&n, a)?;
                    self.stack.push(v);
                } else {
                    match self.get_var(&n) {
                        Some(Val::Function(target)) => {
                            let def = if let Some(def) = self.funcs.get(&target).cloned() {
                                def
                            } else if let Some(def) = resolve_function_object(&target)? {
                                def
                            } else {
                                return Err(format!("dangling function reference '{target}'"));
                            };
                            self.invoke_in_execute(def, a)?;
                        }
                        Some(val) if crate::py_bridge::is_py_handle(&val) => {
                            let res = crate::py_bridge::py_call(&val, &a, None)?;
                            self.stack.push(res);
                        }
                        Some(Val::Object(id)) => {
                            let class_name = {
                                let heap =
                                    self.objects.lock().map_err(|_| "object heap poisoned")?;
                                heap.get(&id).ok_or("dangling object")?.class_name.clone()
                            };
                            let method = self
                                .find_method_cached(&class_name, "__call__")
                                .ok_or(format!("'{class_name}' object is not callable"))?;
                            let mut call_args = vec![Val::Object(id)];
                            call_args.extend(a);
                            self.invoke_in_execute(method, call_args)?;
                        }
                        _ => {
                            let def = self
                                .funcs
                                .get(&n)
                                .cloned()
                                .ok_or(format!("unknown function or class '{n}'"))?;
                            self.invoke_in_execute(def, a)?;
                        }
                    }
                }
            }
            Op::CallExpr(k) => {
                let mut a = Vec::with_capacity(k);
                for _ in 0..k {
                    a.push(self.pop()?);
                }
                a.reverse();
                let callee = self.pop()?;
                if crate::py_bridge::is_py_handle(&callee) {
                    let res = crate::py_bridge::py_call(&callee, &a, None)?;
                    self.stack.push(res);
                } else if let Val::Object(id) = callee {
                    let class_name = {
                        let heap = self.objects.lock().map_err(|_| "object heap poisoned")?;
                        heap.get(&id).ok_or("dangling object")?.class_name.clone()
                    };
                    let method = self
                        .find_method_cached(&class_name, "__call__")
                        .ok_or(format!("'{class_name}' object is not callable"))?;
                    let mut call_args = vec![Val::Object(id)];
                    call_args.extend(a);
                    self.invoke_in_execute(method, call_args)?;
                } else {
                    let (_, def) = self.callable_def(&callee)?;
                    self.invoke_in_execute(def, a)?;
                }
            }
            Op::CallExt(kind, name, specs) => {
                let mut raw_vals = Vec::with_capacity(specs.len());
                for _ in 0..specs.len() {
                    raw_vals.push(self.pop()?);
                }
                raw_vals.reverse();
                let mut pos_args = Vec::new();
                let mut kwargs: Vec<(String, Val)> = Vec::new();
                for ((tag, kw_name), val) in specs.into_iter().zip(raw_vals) {
                    match tag {
                        0 => pos_args.push(val),
                        1 => pos_args.extend(self.iterable_values(&val)?),
                        2 => kwargs.push((kw_name, val)),
                        3 => {
                            let Val::Map(pairs) = val else {
                                return Err("** mapping argument must be Map".into());
                            };
                            for (k, v) in pairs {
                                kwargs.push((k.show(), v));
                            }
                        }
                        _ => return Err("bad CallExt argument spec".into()),
                    }
                }
                match kind {
                    0 => {
                        if let Some(v) =
                            self.call_builtin_kw(&name, pos_args.clone(), kwargs.clone())?
                        {
                            self.stack.push(v);
                        } else if self.classes.contains_key(&name) {
                            let v = self.instantiate_kw(&name, pos_args, kwargs)?;
                            self.stack.push(v);
                        } else {
                            match self.get_var(&name) {
                                Some(Val::Function(target)) => {
                                    let def = if let Some(def) = self.funcs.get(&target).cloned() {
                                        def
                                    } else if let Some(def) = resolve_function_object(&target)? {
                                        def
                                    } else {
                                        return Err(format!(
                                            "dangling function reference '{target}'"
                                        ));
                                    };
                                    self.invoke_in_execute_kw(def, pos_args, kwargs)?;
                                }
                                Some(val) if crate::py_bridge::is_py_handle(&val) => {
                                    let kw_map = Val::Map(
                                        kwargs.into_iter().map(|(k, v)| (Val::Str(k), v)).collect(),
                                    );
                                    let res =
                                        crate::py_bridge::py_call(&val, &pos_args, Some(&kw_map))?;
                                    self.stack.push(res);
                                }
                                Some(Val::Object(id)) => {
                                    let class_name = {
                                        let heap = self
                                            .objects
                                            .lock()
                                            .map_err(|_| "object heap poisoned")?;
                                        heap.get(&id).ok_or("dangling object")?.class_name.clone()
                                    };
                                    let method =
                                        self.find_method_cached(&class_name, "__call__").ok_or(
                                            format!("'{class_name}' object is not callable"),
                                        )?;
                                    let mut call_args = vec![Val::Object(id)];
                                    call_args.extend(pos_args);
                                    self.invoke_in_execute_kw(method, call_args, kwargs)?;
                                }
                                _ => {
                                    let def = self
                                        .funcs
                                        .get(&name)
                                        .cloned()
                                        .ok_or(format!("unknown function or class '{name}'"))?;
                                    self.invoke_in_execute_kw(def, pos_args, kwargs)?;
                                }
                            }
                        }
                    }
                    1 => {
                        let receiver = self.pop()?;
                        if crate::py_bridge::is_py_handle(&receiver) {
                            let kw_map = Val::Map(
                                kwargs.into_iter().map(|(k, v)| (Val::Str(k), v)).collect(),
                            );
                            let res = crate::py_bridge::py_call_method(
                                &receiver,
                                &name,
                                &pos_args,
                                Some(&kw_map),
                            )?;
                            self.stack.push(res);
                        } else if let Val::Object(id) = receiver {
                            let class_name = {
                                let heap =
                                    self.objects.lock().map_err(|_| "object heap poisoned")?;
                                heap.get(&id).ok_or("dangling object")?.class_name.clone()
                            };
                            let method = self
                                .find_method_cached(&class_name, &name)
                                .ok_or(format!("undefined method '{name}' on {class_name}"))?;
                            let mut call_args = vec![Val::Object(id)];
                            call_args.extend(pos_args);
                            self.invoke_in_execute_kw(method, call_args, kwargs)?;
                        } else if let Val::List(items) = receiver {
                            if name == "sort" && pos_args.is_empty() {
                                let mut kw = kwargs;
                                let key_fn = Self::pop_kw(&mut kw, "key")
                                    .filter(|v| !matches!(v, Val::Null));
                                let reverse = match Self::pop_kw(&mut kw, "reverse") {
                                    Some(v) => v.truthy(),
                                    None => false,
                                };
                                if let Some((bad, _)) = kw.first() {
                                    return Err(format!(
                                        "list.sort got unexpected keyword argument '{bad}'"
                                    ));
                                }
                                let sorted = self.sorted_with_opts(items, key_fn, reverse)?;
                                self.stack.push(Val::List(sorted));
                            } else {
                                return Err(format!(
                                    "List method '{name}' does not support keyword arguments"
                                ));
                            }
                        } else if let Val::Map(pairs) = receiver {
                            if let Some((_, member)) =
                                pairs.iter().find(|(k, _)| *k == Val::Str(name.clone()))
                            {
                                if crate::py_bridge::is_py_handle(member) {
                                    let kw_map = Val::Map(
                                        kwargs.into_iter().map(|(k, v)| (Val::Str(k), v)).collect(),
                                    );
                                    let res = crate::py_bridge::py_call(
                                        member,
                                        &pos_args,
                                        Some(&kw_map),
                                    )?;
                                    self.stack.push(res);
                                } else {
                                    let (_, def) = self.callable_def(member)?;
                                    self.invoke_in_execute_kw(def, pos_args, kwargs)?;
                                }
                            } else {
                                return Err(format!("undefined module member '{name}'"));
                            }
                        } else {
                            return Err("keyword method call requires object/module/list".into());
                        }
                    }
                    _ => {
                        let callee = self.pop()?;
                        if crate::py_bridge::is_py_handle(&callee) {
                            let kw_map = Val::Map(
                                kwargs.into_iter().map(|(k, v)| (Val::Str(k), v)).collect(),
                            );
                            let res = crate::py_bridge::py_call(&callee, &pos_args, Some(&kw_map))?;
                            self.stack.push(res);
                        } else if let Val::Object(id) = callee {
                            let class_name = {
                                let heap =
                                    self.objects.lock().map_err(|_| "object heap poisoned")?;
                                heap.get(&id).ok_or("dangling object")?.class_name.clone()
                            };
                            let method = self
                                .find_method_cached(&class_name, "__call__")
                                .ok_or(format!("'{class_name}' object is not callable"))?;
                            let mut call_args = vec![Val::Object(id)];
                            call_args.extend(pos_args);
                            self.invoke_in_execute_kw(method, call_args, kwargs)?;
                        } else {
                            let (_, def) = self.callable_def(&callee)?;
                            self.invoke_in_execute_kw(def, pos_args, kwargs)?;
                        }
                    }
                }
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
                let mut referenced = HashSet::new();
                collect_referenced_names(&code, &mut referenced);
                let param_named: Vec<bool> =
                    params.iter().map(|(p, _)| referenced.contains(p)).collect();
                for (p, _) in &params {
                    referenced.remove(p);
                }
                let captured_upvalues = self.capture_upvalues(&referenced);
                let debug_scope = self.child_debug_scope(self.ip - 1, 0);
                let def = self.build_function_def(
                    params,
                    return_type,
                    is_async,
                    code,
                    name.clone(),
                    debug_scope,
                    Arc::new(captured_upvalues),
                    param_named,
                );
                Arc::make_mut(&mut self.funcs).insert(name, def);
            }
            Op::DefClass(name, parent, methods) => {
                if let Some(ref p) = parent {
                    if !self.classes.contains_key(p) {
                        return Err(format!("unknown parent class '{p}'"));
                    }
                }
                self.method_cache.clear();
                let mut referenced = HashSet::new();
                let mut method_named = Vec::with_capacity(methods.len());
                for (_, params, _, _, body) in &methods {
                    let mut m_refs = HashSet::new();
                    collect_referenced_names(body, &mut m_refs);
                    let named: Vec<bool> = params.iter().map(|(p, _)| m_refs.contains(p)).collect();
                    for (p, _) in params {
                        m_refs.remove(p);
                    }
                    referenced.extend(m_refs);
                    method_named.push(named);
                }
                let upvalues = Arc::new(self.capture_upvalues(&referenced));
                let mut compiled = HashMap::new();
                for (sub_idx, ((method, params, return_type, is_async, code), param_named)) in
                    methods.into_iter().zip(method_named).enumerate()
                {
                    let debug_scope = self.child_debug_scope(self.ip - 1, sub_idx);
                    let debug_name = format!("{name}.{method}");
                    let def = self.build_function_def(
                        params,
                        return_type,
                        is_async,
                        code,
                        debug_name,
                        debug_scope,
                        upvalues.clone(),
                        param_named,
                    );
                    compiled.insert(method, def);
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
                let val = self.pop()?;
                return self.pop_frame_or_finish(val, "return");
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
                return self.pop_frame_or_finish(Val::Null, "return");
            }
        }
        Ok(None)
    }
}
