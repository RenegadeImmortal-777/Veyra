// The stack virtual machine: executes the compiled `Op` stream.
//
// Extracted from `main.rs` without behavior change; see docs/ENGINE-SPLIT.md.
// `execute()` is still one function with one giant match; separating the 212
// inline builtins out of `Op::Call` is Phase 2 of the split.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
};

use crate::bytecode::{
    ClassDef, FunctionDef, ObjectData, Op, code_has_yield, collect_referenced_names,
    compute_c3_mro, compute_local_count,
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
    active_class: Option<String>,
    active_self: Option<Val>,
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
    active_class: Option<String>,
    active_self: Option<Val>,
}

pub(crate) struct GcStatsInner {
    pub(crate) collections: usize,
    pub(crate) total_collected: usize,
    pub(crate) last_collected: usize,
    pub(crate) next_threshold: usize,
}

impl Default for GcStatsInner {
    fn default() -> Self {
        Self {
            collections: 0,
            total_collected: 0,
            last_collected: 0,
            next_threshold: 4096,
        }
    }
}

pub(crate) struct GcState {
    pub(crate) paused: AtomicBool,
    pub(crate) allocs_since_gc: AtomicUsize,
    pub(crate) inner: Mutex<GcStatsInner>,
}

impl Default for GcState {
    fn default() -> Self {
        Self {
            paused: AtomicBool::new(false),
            allocs_since_gc: AtomicUsize::new(0),
            inner: Mutex::new(GcStatsInner::default()),
        }
    }
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
    pub(crate) method_cache: HashMap<(String, String), (String, FunctionDef)>,
    pub(crate) objects: Arc<Mutex<HashMap<u64, ObjectData>>>,
    pub(crate) class_objects: Arc<Mutex<HashMap<String, u64>>>,
    pub(crate) gc: Arc<GcState>,
    pub(crate) handlers: Vec<(usize, String, usize)>,
    pub(crate) breakpoints: HashSet<usize>,
    pub(crate) trace: bool,
    pub(crate) finished: bool,
    pub(crate) debug_scope: usize,
    pub(crate) debugger: Option<Arc<DebugSession>>,
    pub(crate) call_counts: HashMap<usize, u32>,
    pub(crate) active_class: Option<String>,
    pub(crate) active_self: Option<Val>,
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
    fn builtin_classes_map() -> HashMap<String, ClassDef> {
        let mut classes = HashMap::new();
        let make = |bases: Vec<String>, mro: Vec<String>| ClassDef {
            bases,
            metaclass: None,
            mro,
            attrs: HashMap::new(),
            methods: HashMap::new(),
        };
        classes.insert("object".into(), make(vec![], vec!["object".into()]));
        for name in [
            "type", "int", "float", "str", "bool", "list", "dict", "function", "null",
        ] {
            classes.insert(
                name.into(),
                make(vec!["object".into()], vec![name.into(), "object".into()]),
            );
        }
        classes
    }
    pub(crate) fn ensure_builtin_classes(&mut self) {
        if !self.classes.contains_key("object") || !self.classes.contains_key("type") {
            let defaults = Self::builtin_classes_map();
            let map = Arc::make_mut(&mut self.classes);
            for (k, v) in defaults {
                map.entry(k).or_insert(v);
            }
        }
    }
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
            classes: Arc::new(Self::builtin_classes_map()),
            method_cache: HashMap::new(),
            objects: Arc::new(Mutex::new(HashMap::new())),
            class_objects: Arc::new(Mutex::new(HashMap::new())),
            gc: Arc::new(GcState::default()),
            handlers: vec![],
            breakpoints: HashSet::new(),
            trace: false,
            finished: false,
            debug_scope: 0,
            debugger: None,
            call_counts: HashMap::new(),
            active_class: None,
            active_self: None,
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
            class_objects: self.class_objects.clone(),
            gc: self.gc.clone(),
            handlers: vec![],
            breakpoints: HashSet::new(),
            trace: false,
            finished: false,
            debug_scope: prepared.debug_scope,
            debugger: None,
            call_counts: HashMap::new(),
            active_class: prepared.active_class,
            active_self: prepared.active_self,
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
    pub(crate) fn class_is_a(&self, class_name: &str, target: &str) -> bool {
        if class_name == target || target == "object" {
            return true;
        }
        if let Some(cls) = self.classes.get(class_name) {
            if !cls.mro.is_empty() {
                return cls.mro.iter().any(|c| c == target);
            }
            return cls.bases.iter().any(|b| self.class_is_a(b, target));
        }
        false
    }
    pub(crate) fn value_matches(&self, value: &Val, ty: &str) -> bool {
        match ty {
            "Any" | "object" => true,
            "Number" | "Float" | "float" => matches!(value, Val::Num(_)),
            "Int" | "int" => matches!(value, Val::Num(n) if n.fract() == 0.0),
            "String" | "str" => matches!(value, Val::Str(_)),
            "Bool" | "bool" => matches!(value, Val::Bool(_)),
            "List" | "list" => matches!(value, Val::List(_)),
            "Map" | "dict" => matches!(value, Val::Map(_)),
            "Null" | "Void" | "null" => matches!(value, Val::Null),
            "Object" => matches!(value, Val::Object(_)),
            "Function" | "function" => matches!(value, Val::Function(_)),
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
    fn class_mro_slice(&self, class_name: &str) -> Vec<String> {
        if let Some(cls) = self.classes.get(class_name) {
            if !cls.mro.is_empty() {
                return cls.mro.clone();
            }
        }
        vec![class_name.to_string(), "object".to_string()]
    }
    fn find_method_with_owner(
        &self,
        class_name: &str,
        method: &str,
    ) -> Option<(String, FunctionDef)> {
        let mro = self.class_mro_slice(class_name);
        for name in mro {
            if let Some(class) = self.classes.get(&name) {
                if let Some(def) = class.methods.get(method) {
                    return Some((name, def.clone()));
                }
            }
        }
        None
    }
    pub(crate) fn find_method_with_owner_cached(
        &mut self,
        class_name: &str,
        method: &str,
    ) -> Option<(String, FunctionDef)> {
        let key = (class_name.to_string(), method.to_string());
        if let Some(cached) = self.method_cache.get(&key) {
            return Some(cached.clone());
        }
        let found = self.find_method_with_owner(class_name, method)?;
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
            active_class: None,
            active_self: None,
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
            active_class: std::mem::replace(&mut self.active_class, prepared.active_class),
            active_self: std::mem::replace(&mut self.active_self, prepared.active_self),
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
        self.active_class = caller.active_class;
        self.active_self = caller.active_self;
    }
    pub(crate) fn resolve_jit_callee(&self, fname: &str) -> Option<FunctionDef> {
        match self.get_var(fname) {
            Some(Val::Function(target)) => self
                .funcs
                .get(&target)
                .cloned()
                .or_else(|| resolve_function_object(&target).ok().flatten()),
            _ => self.funcs.get(fname).cloned(),
        }
    }
    fn invoke_in_execute(&mut self, def: FunctionDef, args: Vec<Val>) -> Result<(), String> {
        self.invoke_in_execute_kw_ctx(def, args, Vec::new(), None, None)
    }
    fn invoke_in_execute_kw(
        &mut self,
        def: FunctionDef,
        args: Vec<Val>,
        kwargs: Vec<(String, Val)>,
    ) -> Result<(), String> {
        self.invoke_in_execute_kw_ctx(def, args, kwargs, None, None)
    }
    fn invoke_in_execute_kw_ctx(
        &mut self,
        def: FunctionDef,
        args: Vec<Val>,
        kwargs: Vec<(String, Val)>,
        active_class: Option<String>,
        active_self: Option<Val>,
    ) -> Result<(), String> {
        if def.is_async || def.is_generator {
            let v = self.call_def_kw_ctx(def, args, kwargs, active_class, active_self)?;
            self.stack.push(v);
            return Ok(());
        }
        if kwargs.is_empty()
            && !self.trace
            && self.breakpoints.is_empty()
            && self.debugger.is_none()
        {
            let ptr_key = Arc::as_ptr(&def.code) as usize;
            let count = {
                let entry = self.call_counts.entry(ptr_key).or_insert(0);
                *entry = entry.saturating_add(1);
                *entry
            };
            if let Some(res) =
                crate::jit::jit_try_run(&def.debug_name, &def, &args, count, &|fname| {
                    self.resolve_jit_callee(fname)
                })
            {
                self.stack.push(res);
                return Ok(());
            }
        }
        let mut prepared = self.prepare_call_kw(def, args, kwargs)?;
        prepared.active_class = active_class;
        prepared.active_self = active_self;
        self.push_prepared_frame(prepared);
        Ok(())
    }
    fn call_def_kw_ctx(
        &mut self,
        def: FunctionDef,
        args: Vec<Val>,
        kwargs: Vec<(String, Val)>,
        active_class: Option<String>,
        active_self: Option<Val>,
    ) -> Result<Val, String> {
        if def.is_async {
            if def.is_generator {
                return Err("async generator functions are not yet supported".into());
            }
            let mut prepared = self.prepare_call_kw(def, args, kwargs)?;
            prepared.active_class = active_class;
            prepared.active_self = active_self;
            return self.spawn_prepared_task(prepared, "async");
        }
        let mut prepared = self.prepare_call_kw(def, args, kwargs)?;
        prepared.active_class = active_class;
        prepared.active_self = active_self;
        self.run_prepared_call_mut(prepared)
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
    fn exec_prepared_nested(
        caller: &Vm,
        nested: &mut Vm,
        is_generator: bool,
        return_type: Option<String>,
        debug_name: String,
        debug_scope: usize,
    ) -> Result<Val, String> {
        if is_generator {
            if let Some(expected) = return_type {
                if expected != "Generator" && expected != "Any" {
                    return Err(format!(
                        "generator function return type must be Generator or Any, got {expected}"
                    ));
                }
            }
            nested.debugger = caller.debugger.clone();
            let id = GENERATOR_IDS.fetch_add(1, Ordering::Relaxed);
            generators()
                .lock()
                .map_err(|_| "generator registry poisoned")?
                .insert(id, nested.clone_generator_state());
            return Ok(Val::Generator(id));
        }
        let _frame_guard = if let Some(dbg) = &caller.debugger {
            dbg.update_caller_frame(
                caller.debug_scope,
                caller.ip.saturating_sub(1),
                &caller.vars,
                &caller.cells,
                &caller.locals,
                &caller.local_defined,
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
            if !nested.value_matches(&result, &expected) {
                return Err(format!(
                    "return value expected {expected}, got {}",
                    result.show()
                ));
            }
        }
        Ok(result)
    }
    fn clone_generator_state(&self) -> Vm {
        Self {
            code: self.code.clone(),
            ip: self.ip,
            stack: self.stack.clone(),
            vars: self.vars.clone(),
            cells: self.cells.clone(),
            var_types: self.var_types.clone(),
            consts: self.consts.clone(),
            locals: self.locals.clone(),
            local_defined: self.local_defined.clone(),
            local_consts: self.local_consts.clone(),
            local_types: self.local_types.clone(),
            return_type: self.return_type.clone(),
            frames: Vec::new(),
            funcs: self.funcs.clone(),
            classes: self.classes.clone(),
            method_cache: HashMap::new(),
            objects: self.objects.clone(),
            class_objects: self.class_objects.clone(),
            gc: self.gc.clone(),
            handlers: self.handlers.clone(),
            breakpoints: self.breakpoints.clone(),
            trace: self.trace,
            finished: self.finished,
            debug_scope: self.debug_scope,
            debugger: self.debugger.clone(),
            call_counts: HashMap::new(),
            active_class: self.active_class.clone(),
            active_self: self.active_self.clone(),
        }
    }
    fn run_prepared_call(&self, prepared: PreparedCall) -> Result<Val, String> {
        let is_generator = prepared.is_generator;
        let return_type = prepared.return_type.clone();
        let debug_name = prepared.debug_name.clone();
        let debug_scope = prepared.debug_scope;
        let mut nested = self.nested_from_prepared(prepared);
        Self::exec_prepared_nested(
            self,
            &mut nested,
            is_generator,
            return_type,
            debug_name,
            debug_scope,
        )
    }
    fn run_prepared_call_mut(&mut self, prepared: PreparedCall) -> Result<Val, String> {
        let is_generator = prepared.is_generator;
        let return_type = prepared.return_type.clone();
        let debug_name = prepared.debug_name.clone();
        let debug_scope = prepared.debug_scope;
        let mut nested = self.nested_from_prepared(prepared);
        let res = Self::exec_prepared_nested(
            self,
            &mut nested,
            is_generator,
            return_type,
            debug_name,
            debug_scope,
        )?;
        if !Arc::ptr_eq(&self.funcs, &nested.funcs) {
            self.funcs = nested.funcs;
        }
        if !Arc::ptr_eq(&self.classes, &nested.classes) {
            self.classes = nested.classes;
            self.method_cache.clear();
        }
        Ok(res)
    }
    pub(crate) fn instantiate(&mut self, class_name: &str, args: Vec<Val>) -> Result<Val, String> {
        self.instantiate_kw(class_name, args, Vec::new())
    }
    pub(crate) fn instantiate_kw(
        &mut self,
        class_name: &str,
        args: Vec<Val>,
        kwargs: Vec<(String, Val)>,
    ) -> Result<Val, String> {
        match class_name {
            "int" | "float" | "str" | "bool" | "list" | "dict" | "type" => {
                return self
                    .call_builtin_kw(class_name, args, kwargs)?
                    .ok_or_else(|| format!("builtin '{class_name}' did not produce a value"));
            }
            "object" => {
                if !args.is_empty() || !kwargs.is_empty() {
                    return Err("object() takes no arguments".into());
                }
                return self.alloc_object("object", HashMap::new());
            }
            _ => {}
        }
        let meta_name = self
            .classes
            .get(class_name)
            .ok_or_else(|| format!("unknown class '{class_name}'"))?
            .metaclass
            .clone();
        if let Some(meta) = meta_name {
            if let Some((owner, def)) = self.find_method_with_owner_cached(&meta, "__call__") {
                if owner != "type" && owner != "object" {
                    let cls_obj = self.class_object_val(class_name)?;
                    let mut call_args = Vec::with_capacity(1 + args.len());
                    call_args.push(cls_obj.clone());
                    call_args.extend(args);
                    return self.call_def_kw_ctx(
                        def,
                        call_args,
                        kwargs,
                        Some(owner),
                        Some(cls_obj),
                    );
                }
            }
        }
        self.default_instantiate_kw(class_name, args, kwargs)
    }
    fn default_instantiate_kw(
        &mut self,
        class_name: &str,
        args: Vec<Val>,
        kwargs: Vec<(String, Val)>,
    ) -> Result<Val, String> {
        if !self.classes.contains_key(class_name) {
            return Err(format!("unknown class '{class_name}'"));
        }
        let mro = self.class_mro_slice(class_name);
        let mut custom_new: Option<(String, FunctionDef)> = None;
        for c in &mro {
            if c == "object" || c == "type" {
                continue;
            }
            if let Some(cls) = self.classes.get(c) {
                if let Some(def) = cls.methods.get("__new__") {
                    custom_new = Some((c.clone(), def.clone()));
                    break;
                }
            }
        }
        let mut custom_new_ran = false;
        let inst = if let Some((new_owner, new_def)) = custom_new {
            custom_new_ran = true;
            let cls_obj = self.class_object_val(class_name)?;
            let mut new_args = Vec::with_capacity(1 + args.len());
            new_args.push(cls_obj.clone());
            new_args.extend(args.clone());
            let created = self.call_def_kw_ctx(
                new_def,
                new_args,
                kwargs.clone(),
                Some(new_owner),
                Some(cls_obj),
            )?;
            let is_instance = match created {
                Val::Object(id) => self
                    .object_class_name(id)
                    .map(|c| self.class_is_a(&c, class_name))
                    .unwrap_or(false),
                _ => false,
            };
            if !is_instance {
                return Ok(created);
            }
            created
        } else {
            self.alloc_object(class_name, HashMap::new())?
        };

        let mut init_found: Option<(String, FunctionDef)> = None;
        for c in &mro {
            if c == "object" || c == "type" {
                continue;
            }
            if let Some(cls) = self.classes.get(c) {
                if let Some(def) = cls
                    .methods
                    .get("__init__")
                    .or_else(|| cls.methods.get("init"))
                {
                    init_found = Some((c.clone(), def.clone()));
                    break;
                }
            }
        }
        if let Some((init_owner, init_def)) = init_found {
            let mut call_args = vec![inst.clone()];
            call_args.extend(args);
            let _ = self.call_def_kw_ctx(
                init_def,
                call_args,
                kwargs,
                Some(init_owner),
                Some(inst.clone()),
            )?;
        } else if !custom_new_ran && (!args.is_empty() || !kwargs.is_empty()) {
            return Err(format!("{class_name} has no init method"));
        }
        Ok(inst)
    }
    pub(crate) fn try_dunder(
        &mut self,
        id: u64,
        method: &str,
        args: Vec<Val>,
    ) -> Result<Option<Val>, String> {
        let class_name = self.object_class_name(id)?;
        let Some((owner, def)) = self.find_method_with_owner_cached(&class_name, method) else {
            return Ok(None);
        };
        let self_val = Val::Object(id);
        let mut call_args = vec![self_val.clone()];
        call_args.extend(args);
        Ok(Some(self.call_def_kw_ctx(
            def,
            call_args,
            Vec::new(),
            Some(owner),
            Some(self_val),
        )?))
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
        } else if crate::runtime::try_capture_print(&format!("{rendered}{end}")) {
            return Ok(Val::Null);
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
        self.call_val_kw_sync(callee, args, Vec::new())
    }
    pub(crate) fn is_callable_val(&mut self, val: &Val) -> bool {
        if crate::py_bridge::is_py_handle(val) {
            return false;
        }
        match val {
            Val::Function(_) => true,
            Val::Object(id) => {
                let Ok((cls, is_class_obj, is_bound)) =
                    (|| -> Result<(String, bool, bool), String> {
                        let heap = self.objects.lock().map_err(|_| "object heap poisoned")?;
                        let o = heap.get(id).ok_or("dangling object")?;
                        Ok((
                            o.class_name.clone(),
                            o.fields.contains_key("__veyra_class__"),
                            o.class_name == "__bound_method__",
                        ))
                    })()
                else {
                    return false;
                };
                if is_class_obj || is_bound {
                    return true;
                }
                if cls == "__property__"
                    || cls == "__staticmethod__"
                    || cls == "__classmethod__"
                    || cls == "__super__"
                {
                    return false;
                }
                self.find_method_with_owner_cached(&cls, "__call__")
                    .is_some()
            }
            _ => false,
        }
    }
    fn object_class_name(&self, id: u64) -> Result<String, String> {
        let heap = self.objects.lock().map_err(|_| "object heap poisoned")?;
        Ok(heap.get(&id).ok_or("dangling object")?.class_name.clone())
    }
    fn object_data_snapshot(&self, id: u64) -> Result<ObjectData, String> {
        let heap = self.objects.lock().map_err(|_| "object heap poisoned")?;
        heap.get(&id)
            .cloned()
            .ok_or_else(|| "dangling object".into())
    }
    fn alloc_object(&self, class_name: &str, fields: HashMap<String, Val>) -> Result<Val, String> {
        let id = OBJECT_IDS.fetch_add(1, Ordering::Relaxed);
        self.objects
            .lock()
            .map_err(|_| "object heap poisoned")?
            .insert(
                id,
                ObjectData {
                    class_name: class_name.to_string(),
                    fields,
                },
            );
        self.gc.allocs_since_gc.fetch_add(1, Ordering::Relaxed);
        Ok(Val::Object(id))
    }
    pub(crate) fn class_object_tag(&self, val: &Val) -> Result<Option<String>, String> {
        let Val::Object(id) = val else {
            return Ok(None);
        };
        let heap = self.objects.lock().map_err(|_| "object heap poisoned")?;
        Ok(heap
            .get(id)
            .and_then(|o| match o.fields.get("__veyra_class__") {
                Some(Val::Str(name)) => Some(name.clone()),
                _ => None,
            }))
    }
    pub(crate) fn resolve_class_name_arg(&self, val: &Val) -> Result<String, String> {
        if let Some(cls) = self.class_object_tag(val)? {
            return Ok(cls);
        }
        if let Val::Str(name) = val {
            if self.classes.contains_key(name) {
                return Ok(name.clone());
            }
        }
        Err(format!("expected class/type, got {}", val.show()))
    }
    pub(crate) fn class_object_val(&mut self, cls_name: &str) -> Result<Val, String> {
        self.ensure_builtin_classes();
        if !self.classes.contains_key(cls_name) {
            return Err(format!("unknown class '{cls_name}'"));
        }
        {
            let cache = self
                .class_objects
                .lock()
                .map_err(|_| "class object table poisoned")?;
            if let Some(&id) = cache.get(cls_name) {
                let heap = self.objects.lock().map_err(|_| "object heap poisoned")?;
                if heap.contains_key(&id) {
                    return Ok(Val::Object(id));
                }
            }
        }
        let meta_name = self
            .classes
            .get(cls_name)
            .and_then(|c| c.metaclass.clone())
            .unwrap_or_else(|| "type".to_string());
        let mut fields = HashMap::new();
        fields.insert(
            "__veyra_class__".to_string(),
            Val::Str(cls_name.to_string()),
        );
        fields.insert("__name__".to_string(), Val::Str(cls_name.to_string()));
        let obj = self.alloc_object(&meta_name, fields)?;
        if let Val::Object(id) = obj {
            self.class_objects
                .lock()
                .map_err(|_| "class object table poisoned")?
                .insert(cls_name.to_string(), id);
        }
        Ok(obj)
    }
    fn make_bound_method(
        &self,
        func: Val,
        self_val: Val,
        owner: Option<&str>,
    ) -> Result<Val, String> {
        let mut fields = HashMap::new();
        fields.insert("__func__".to_string(), func);
        fields.insert("__self__".to_string(), self_val);
        if let Some(o) = owner {
            fields.insert("__owner__".to_string(), Val::Str(o.to_string()));
        }
        self.alloc_object("__bound_method__", fields)
    }
    pub(crate) fn make_property_obj(&self, fget: Val, fset: Val, fdel: Val) -> Result<Val, String> {
        let mut fields = HashMap::new();
        fields.insert("fget".to_string(), fget);
        fields.insert("fset".to_string(), fset);
        fields.insert("fdel".to_string(), fdel);
        self.alloc_object("__property__", fields)
    }
    pub(crate) fn make_descriptor_wrapper(&self, tag: &str, func: Val) -> Result<Val, String> {
        let mut fields = HashMap::new();
        fields.insert("__func__".to_string(), func);
        self.alloc_object(tag, fields)
    }
    pub(crate) fn make_super_obj(&mut self, args: &[Val]) -> Result<Val, String> {
        let (thisclass, self_val) = if args.is_empty() {
            let cls = self
                .active_class
                .clone()
                .ok_or_else(|| "super(): no active class context".to_string())?;
            let sv = self
                .active_self
                .clone()
                .ok_or_else(|| "super(): no active self/cls context".to_string())?;
            (cls, sv)
        } else if args.len() == 2 {
            let cls = self.resolve_class_name_arg(&args[0])?;
            (cls, args[1].clone())
        } else {
            return Err("super() expects 0 or 2 arguments (cls, self_or_cls)".into());
        };
        let mut fields = HashMap::new();
        fields.insert("__thisclass__".to_string(), Val::Str(thisclass));
        fields.insert("__self__".to_string(), self_val);
        self.alloc_object("__super__", fields)
    }
    fn find_member_in_mro_slice(
        &self,
        mro: &[String],
        member: &str,
    ) -> Option<(String, bool, Val, Option<FunctionDef>)> {
        for cls_name in mro {
            if let Some(cls) = self.classes.get(cls_name) {
                if let Some(v) = cls.attrs.get(member) {
                    return Some((cls_name.clone(), true, v.clone(), None));
                }
                if let Some(def) = cls.methods.get(member) {
                    return Some((cls_name.clone(), false, Val::Null, Some(def.clone())));
                }
            }
        }
        None
    }
    fn descriptor_has_method(&mut self, val: &Val, method: &str) -> Result<Option<String>, String> {
        let Val::Object(id) = val else {
            return Ok(None);
        };
        let cls = self.object_class_name(*id)?;
        if cls == "__property__" {
            return Ok(Some(cls));
        }
        if matches!(
            cls.as_str(),
            "__staticmethod__" | "__classmethod__" | "__bound_method__" | "__super__"
        ) {
            return Ok(None);
        }
        if self.find_method_with_owner_cached(&cls, method).is_some() {
            Ok(Some(cls))
        } else {
            Ok(None)
        }
    }
    fn register_class_from_ns(
        &mut self,
        meta_name: &str,
        cls_name: &str,
        bases_in: &[String],
        ns_pairs: Vec<(Val, Val)>,
    ) -> Result<Val, String> {
        self.ensure_builtin_classes();
        let bases = if bases_in.is_empty() && cls_name != "object" {
            vec!["object".to_string()]
        } else {
            bases_in.to_vec()
        };
        let mro = compute_c3_mro(cls_name, &bases, |b| {
            self.classes
                .get(b)
                .map(|c| {
                    if c.mro.is_empty() {
                        vec![b.to_string(), "object".to_string()]
                    } else {
                        c.mro.clone()
                    }
                })
                .ok_or_else(|| format!("unknown base class '{b}'"))
        })?;
        let mut attrs = HashMap::new();
        let mut methods = HashMap::new();
        for (k, v) in ns_pairs {
            let key = k.show();
            if let Val::Function(ref fk) = v {
                if !fk.starts_with("__builtin__:") {
                    if let Ok((_, def)) = self.callable_def(&v) {
                        methods.insert(key, def);
                        continue;
                    }
                }
            }
            attrs.insert(key, v);
        }
        Arc::make_mut(&mut self.classes).insert(
            cls_name.to_string(),
            ClassDef {
                bases,
                metaclass: if meta_name == "type" {
                    None
                } else {
                    Some(meta_name.to_string())
                },
                mro,
                attrs,
                methods,
            },
        );
        self.method_cache.clear();
        if let Ok(mut cache) = self.class_objects.lock() {
            cache.remove(cls_name);
        }
        self.class_object_val(cls_name)
    }
    pub(crate) fn dynamic_type_create(
        &mut self,
        name: &str,
        bases_val: &Val,
        ns_val: &Val,
    ) -> Result<Val, String> {
        let base_items = self.iterable_values(bases_val)?;
        let mut bases = Vec::with_capacity(base_items.len());
        for b in &base_items {
            bases.push(self.resolve_class_name_arg(b)?);
        }
        let Val::Map(pairs) = ns_val else {
            return Err("type(name, bases, namespace) namespace must be a Map".into());
        };
        self.register_class_from_ns("type", name, &bases, pairs.clone())
    }
    fn invoke_ctx_sync(
        &mut self,
        callable: &Val,
        args: Vec<Val>,
        kwargs: Vec<(String, Val)>,
        active_class: Option<String>,
        active_self: Option<Val>,
    ) -> Result<Val, String> {
        if crate::py_bridge::is_py_handle(callable) {
            if kwargs.is_empty() {
                return crate::py_bridge::py_call(callable, &args, None);
            }
            let kw_map = Val::Map(kwargs.into_iter().map(|(k, v)| (Val::Str(k), v)).collect());
            return crate::py_bridge::py_call(callable, &args, Some(&kw_map));
        }
        if let Val::Function(s) = callable {
            if let Some(bname) = s.strip_prefix("__builtin__:") {
                return self.call_builtin_wrapper_sync(bname, args, kwargs);
            }
        }
        if let Val::Object(id) = callable {
            let snap = self.object_data_snapshot(*id)?;
            if snap.class_name == "__bound_method__" {
                let func = snap
                    .fields
                    .get("__func__")
                    .cloned()
                    .ok_or("corrupt bound method")?;
                let bound_self = snap
                    .fields
                    .get("__self__")
                    .cloned()
                    .ok_or("corrupt bound method")?;
                let owner = snap.fields.get("__owner__").map(|v| v.show());
                let mut full_args = Vec::with_capacity(1 + args.len());
                full_args.push(bound_self.clone());
                full_args.extend(args);
                return self.invoke_ctx_sync(&func, full_args, kwargs, owner, Some(bound_self));
            }
            if let Some(Val::Str(cls_name)) = snap.fields.get("__veyra_class__") {
                return self.instantiate_kw(cls_name, args, kwargs);
            }
            let (owner, def) = self
                .find_method_with_owner_cached(&snap.class_name, "__call__")
                .ok_or_else(|| format!("'{}' object is not callable", snap.class_name))?;
            let mut full_args = Vec::with_capacity(1 + args.len());
            full_args.push(Val::Object(*id));
            full_args.extend(args);
            return self.call_def_kw_ctx(
                def,
                full_args,
                kwargs,
                Some(owner),
                Some(Val::Object(*id)),
            );
        }
        let (_, def) = self.callable_def(callable)?;
        self.call_def_kw_ctx(def, args, kwargs, active_class, active_self)
    }
    pub(crate) fn call_val_kw_sync(
        &mut self,
        callee: &Val,
        args: Vec<Val>,
        kwargs: Vec<(String, Val)>,
    ) -> Result<Val, String> {
        self.invoke_ctx_sync(callee, args, kwargs, None, None)
    }
    fn call_builtin_wrapper_sync(
        &mut self,
        bname: &str,
        args: Vec<Val>,
        kwargs: Vec<(String, Val)>,
    ) -> Result<Val, String> {
        match bname {
            "__prop_getter__" | "__prop_setter__" | "__prop_deleter__" => {
                if args.len() != 2 {
                    return Err(format!("property.{bname} expects 1 callable argument"));
                }
                let Val::Object(pid) = args[0] else {
                    return Err("property modifier target must be property".into());
                };
                let mut fields = self.object_data_snapshot(pid)?.fields;
                let key = match bname {
                    "__prop_getter__" => "fget",
                    "__prop_setter__" => "fset",
                    _ => "fdel",
                };
                fields.insert(key.to_string(), args[1].clone());
                self.alloc_object("__property__", fields)
            }
            "__object_new__" => {
                let cls_arg = args
                    .first()
                    .ok_or("object.__new__(cls) requires a class argument")?;
                let cls_name = self.resolve_class_name_arg(cls_arg)?;
                self.alloc_object(&cls_name, HashMap::new())
            }
            "__object_init__" => Ok(Val::Null),
            "__object_setattr__" => {
                if args.len() != 3 {
                    return Err("object.__setattr__ expects (self, name, value)".into());
                }
                let Val::Object(id) = args[0] else {
                    return Err("object.__setattr__ target must be an Object".into());
                };
                let attr = match &args[1] {
                    Val::Str(s) => s.clone(),
                    _ => return Err("attribute name must be String".into()),
                };
                let mut heap = self.objects.lock().map_err(|_| "object heap poisoned")?;
                let data = heap.get_mut(&id).ok_or("dangling object")?;
                data.fields.insert(attr, args[2].clone());
                Ok(Val::Null)
            }
            "__object_delattr__" => {
                if args.len() != 2 {
                    return Err("object.__delattr__ expects (self, name)".into());
                }
                let Val::Object(id) = args[0] else {
                    return Err("object.__delattr__ target must be an Object".into());
                };
                let attr = match &args[1] {
                    Val::Str(s) => s.as_str(),
                    _ => return Err("attribute name must be String".into()),
                };
                let mut heap = self.objects.lock().map_err(|_| "object heap poisoned")?;
                let data = heap.get_mut(&id).ok_or("dangling object")?;
                data.fields
                    .remove(attr)
                    .ok_or_else(|| format!("undefined field '{attr}'"))?;
                Ok(Val::Null)
            }
            "__type_call__" => {
                if args.is_empty() {
                    return Err("type.__call__ expects (cls, *args)".into());
                }
                let cls_name = self.resolve_class_name_arg(&args[0])?;
                self.default_instantiate_kw(&cls_name, args[1..].to_vec(), kwargs)
            }
            "__type_new__" => {
                if args.len() != 4 {
                    return Err("type.__new__ expects (mcs, name, bases, namespace)".into());
                }
                let meta_name = self.resolve_class_name_arg(&args[0])?;
                let Val::Str(ref cls_name) = args[1] else {
                    return Err("type.__new__ name must be String".into());
                };
                let base_vals = self.iterable_values(&args[2])?;
                let mut bases = Vec::with_capacity(base_vals.len());
                for b in &base_vals {
                    bases.push(self.resolve_class_name_arg(b)?);
                }
                let Val::Map(ref pairs) = args[3] else {
                    return Err("type.__new__ namespace must be Map".into());
                };
                self.register_class_from_ns(&meta_name, cls_name, &bases, pairs.clone())
            }
            "__type_init__" => Ok(Val::Null),
            other => self
                .call_builtin_kw(other, args, kwargs)?
                .ok_or_else(|| format!("unknown builtin '{other}'")),
        }
    }
    fn super_target_info(&mut self, super_id: u64) -> Result<(Vec<String>, Val, bool), String> {
        let snap = self.object_data_snapshot(super_id)?;
        if snap.class_name != "__super__" {
            return Err("internal error: expected __super__ object".into());
        }
        let thisclass = match snap.fields.get("__thisclass__") {
            Some(Val::Str(s)) => s.clone(),
            _ => return Err("corrupt super() object".into()),
        };
        let self_val = snap
            .fields
            .get("__self__")
            .cloned()
            .ok_or("corrupt super() object")?;
        let (target_cls, is_cls_bind) = if let Some(cls_name) = self.class_object_tag(&self_val)? {
            if self.class_is_a(&cls_name, &thisclass) {
                (cls_name, true)
            } else if let Val::Object(oid) = self_val {
                (self.object_class_name(oid)?, false)
            } else {
                (cls_name, true)
            }
        } else if let Val::Object(oid) = self_val {
            (self.object_class_name(oid)?, false)
        } else {
            return Err("super() second argument must be object or class".into());
        };
        let mro = self.class_mro_slice(&target_cls);
        let pos = mro
            .iter()
            .position(|c| c == &thisclass)
            .ok_or_else(|| format!("'{thisclass}' is not in MRO of '{target_cls}'"))?;
        Ok((mro[pos + 1..].to_vec(), self_val, is_cls_bind))
    }
    fn super_get_attr(&mut self, super_id: u64, name: &str) -> Result<Val, String> {
        let (tail_mro, self_val, is_cls_bind) = self.super_target_info(super_id)?;
        if let Some((owner, is_attr, attr_val, method_def)) =
            self.find_member_in_mro_slice(&tail_mro, name)
        {
            return self.materialize_member(
                &owner,
                is_attr,
                attr_val,
                method_def,
                Some(self_val),
                is_cls_bind,
            );
        }
        let is_class_arg = self.class_object_tag(&self_val)?.is_some();
        for c in &tail_mro {
            if c == "object" {
                if let Some(b) = match name {
                    "__new__" => Some("__builtin__:__object_new__"),
                    "__init__" => Some("__builtin__:__object_init__"),
                    "__setattr__" => Some("__builtin__:__object_setattr__"),
                    "__delattr__" => Some("__builtin__:__object_delattr__"),
                    _ => None,
                } {
                    return self.make_bound_method(
                        Val::Function(b.to_string()),
                        self_val,
                        Some("object"),
                    );
                }
            } else if c == "type" && (is_cls_bind || is_class_arg) {
                if let Some(b) = match name {
                    "__call__" => Some("__builtin__:__type_call__"),
                    "__new__" => Some("__builtin__:__type_new__"),
                    "__init__" => Some("__builtin__:__type_init__"),
                    _ => None,
                } {
                    if name == "__new__" {
                        return Ok(Val::Function(b.to_string()));
                    }
                    return self.make_bound_method(
                        Val::Function(b.to_string()),
                        self_val,
                        Some("type"),
                    );
                }
            }
        }
        Err(format!("'super' object has no attribute '{name}'"))
    }
    fn materialize_member(
        &mut self,
        owner: &str,
        is_attr: bool,
        attr_val: Val,
        method_def: Option<FunctionDef>,
        recv_opt: Option<Val>,
        is_cls_bind: bool,
    ) -> Result<Val, String> {
        if is_attr {
            if let Val::Object(did) = attr_val {
                let dcls = self.object_class_name(did)?;
                if dcls == "__staticmethod__" {
                    let df = self.object_data_snapshot(did)?;
                    return df
                        .fields
                        .get("__func__")
                        .cloned()
                        .ok_or_else(|| "corrupt staticmethod".to_string());
                }
                if dcls == "__classmethod__" {
                    let df = self.object_data_snapshot(did)?;
                    let func = df
                        .fields
                        .get("__func__")
                        .cloned()
                        .ok_or_else(|| "corrupt classmethod".to_string())?;
                    let cls_val = match recv_opt {
                        Some(ref r) => {
                            if is_cls_bind {
                                r.clone()
                            } else if let Val::Object(oid) = r {
                                let ocls = self.object_class_name(*oid)?;
                                self.class_object_val(&ocls)?
                            } else {
                                self.class_object_val(owner)?
                            }
                        }
                        None => self.class_object_val(owner)?,
                    };
                    return self.make_bound_method(func, cls_val, Some(owner));
                }
                if dcls == "__property__" {
                    if !is_cls_bind {
                        if let Some(inst) = recv_opt {
                            let df = self.object_data_snapshot(did)?;
                            let fget = df.fields.get("fget").cloned().unwrap_or(Val::Null);
                            if matches!(fget, Val::Null) {
                                return Err("unreadable attribute (property has no getter)".into());
                            }
                            return self.invoke_ctx_sync(
                                &fget,
                                vec![inst.clone()],
                                Vec::new(),
                                Some(owner.to_string()),
                                Some(inst),
                            );
                        }
                    }
                    return Ok(attr_val);
                }
                if let Some((_, get_def)) = self.find_method_with_owner_cached(&dcls, "__get__") {
                    let owner_cls = match recv_opt {
                        Some(ref r) if is_cls_bind => r.clone(),
                        Some(Val::Object(oid)) => {
                            let ocls = self.object_class_name(oid)?;
                            self.class_object_val(&ocls)?
                        }
                        _ => self.class_object_val(owner)?,
                    };
                    let inst_arg = if is_cls_bind {
                        Val::Null
                    } else {
                        recv_opt.unwrap_or(Val::Null)
                    };
                    return self.call_def_kw_ctx(
                        get_def,
                        vec![attr_val, inst_arg, owner_cls],
                        Vec::new(),
                        Some(dcls),
                        Some(Val::Object(did)),
                    );
                }
            }
            return Ok(attr_val);
        }
        let def = method_def.ok_or("missing method def")?;
        let fn_val = Val::Function(export_function_object(def)?);
        if is_cls_bind {
            return Ok(fn_val);
        }
        match recv_opt {
            Some(inst) => self.make_bound_method(fn_val, inst, Some(owner)),
            None => Ok(fn_val),
        }
    }
    pub(crate) fn vm_get_attr(&mut self, obj: &Val, name: &str) -> Result<Val, String> {
        if crate::py_bridge::is_py_handle(obj) {
            return crate::py_bridge::py_get(obj, name);
        }
        let Val::Object(id) = *obj else {
            if let Val::Map(pairs) = obj {
                return pairs
                    .iter()
                    .find(|(k, _)| *k == Val::Str(name.to_string()))
                    .map(|(_, v)| v.clone())
                    .ok_or_else(|| format!("undefined key '{name}'"));
            }
            return Err("member access requires object or map".into());
        };
        let snap = self.object_data_snapshot(id)?;
        if snap.class_name == "__super__" {
            return self.super_get_attr(id, name);
        }
        if snap.class_name == "__property__" {
            if matches!(name, "getter" | "setter" | "deleter") {
                return self.make_bound_method(
                    Val::Function(format!("__builtin__:__prop_{name}__")),
                    Val::Object(id),
                    None,
                );
            }
            if let Some(v) = snap.fields.get(name) {
                return Ok(v.clone());
            }
            return Err(format!("'property' object has no attribute '{name}'"));
        }
        if let Some(Val::Str(cls_name)) = snap.fields.get("__veyra_class__") {
            match name {
                "__name__" => return Ok(Val::Str(cls_name.clone())),
                "__bases__" => {
                    let bases = self
                        .classes
                        .get(cls_name)
                        .map(|c| c.bases.clone())
                        .unwrap_or_default();
                    let mut out = Vec::with_capacity(bases.len());
                    for b in &bases {
                        out.push(self.class_object_val(b)?);
                    }
                    return Ok(Val::List(out));
                }
                "__mro__" => {
                    let mro = self.class_mro_slice(cls_name);
                    let mut out = Vec::with_capacity(mro.len());
                    for c in &mro {
                        out.push(self.class_object_val(c)?);
                    }
                    return Ok(Val::List(out));
                }
                "__dict__" => {
                    let pairs: Vec<(Val, Val)> = self
                        .classes
                        .get(cls_name)
                        .map(|c| {
                            c.attrs
                                .iter()
                                .map(|(k, v)| (Val::Str(k.clone()), v.clone()))
                                .collect()
                        })
                        .unwrap_or_default();
                    return Ok(Val::Map(pairs));
                }
                _ => {}
            }
            let mro = self.class_mro_slice(cls_name);
            if let Some((owner, is_attr, attr_val, method_def)) =
                self.find_member_in_mro_slice(&mro, name)
            {
                return self.materialize_member(
                    &owner,
                    is_attr,
                    attr_val,
                    method_def,
                    Some(Val::Object(id)),
                    true,
                );
            }
            if cls_name == "object" {
                if let Some(b) = match name {
                    "__new__" => Some("__builtin__:__object_new__"),
                    "__init__" => Some("__builtin__:__object_init__"),
                    "__setattr__" => Some("__builtin__:__object_setattr__"),
                    "__delattr__" => Some("__builtin__:__object_delattr__"),
                    _ => None,
                } {
                    return Ok(Val::Function(b.to_string()));
                }
            } else if cls_name == "type" {
                if let Some(b) = match name {
                    "__new__" => Some("__builtin__:__type_new__"),
                    "__init__" => Some("__builtin__:__type_init__"),
                    "__call__" => Some("__builtin__:__type_call__"),
                    _ => None,
                } {
                    return Ok(Val::Function(b.to_string()));
                }
            }
            let meta_mro = self.class_mro_slice(&snap.class_name);
            if let Some((owner, is_attr, attr_val, method_def)) =
                self.find_member_in_mro_slice(&meta_mro, name)
            {
                return self.materialize_member(
                    &owner,
                    is_attr,
                    attr_val,
                    method_def,
                    Some(Val::Object(id)),
                    false,
                );
            }
            return Err(format!(
                "type object '{cls_name}' has no attribute '{name}'"
            ));
        }

        let mro = self.class_mro_slice(&snap.class_name);
        let hit = self.find_member_in_mro_slice(&mro, name);
        if let Some((ref owner, true, ref attr_val, _)) = hit {
            let is_data = self.descriptor_has_method(attr_val, "__set__")?.is_some()
                || self
                    .descriptor_has_method(attr_val, "__delete__")?
                    .is_some();
            if is_data {
                return self.materialize_member(
                    owner,
                    true,
                    attr_val.clone(),
                    None,
                    Some(Val::Object(id)),
                    false,
                );
            }
        }
        if let Some(v) = snap.fields.get(name) {
            return Ok(v.clone());
        }
        if let Some((owner, is_attr, attr_val, method_def)) = hit {
            return self.materialize_member(
                &owner,
                is_attr,
                attr_val,
                method_def,
                Some(Val::Object(id)),
                false,
            );
        }
        if name != "__getattr__" {
            if let Some((owner, def)) =
                self.find_method_with_owner_cached(&snap.class_name, "__getattr__")
            {
                return self.call_def_kw_ctx(
                    def,
                    vec![Val::Object(id), Val::Str(name.to_string())],
                    Vec::new(),
                    Some(owner),
                    Some(Val::Object(id)),
                );
            }
        }
        Err(format!("undefined field '{name}'"))
    }
    pub(crate) fn vm_has_attr(&mut self, obj: &Val, name: &str) -> Result<bool, String> {
        if crate::py_bridge::is_py_handle(obj) {
            return crate::py_bridge::py_hasattr(obj, name);
        }
        Ok(self.vm_get_attr(obj, name).is_ok())
    }
    pub(crate) fn pat_class_pos(
        &mut self,
        val: &Val,
        cls: &Val,
        pos_len: usize,
    ) -> Result<Val, String> {
        let target = self.resolve_class_name_arg(cls)?;
        if !self.value_matches(val, &target) {
            return Ok(Val::Null);
        }
        if pos_len == 0 {
            return Ok(Val::List(Vec::new()));
        }
        if matches!(
            target.as_str(),
            "int" | "float" | "str" | "bool" | "list" | "dict" | "function" | "object"
        ) {
            if pos_len == 1 {
                return Ok(Val::List(vec![val.clone()]));
            }
            return Err(format!(
                "{target}() accepts at most 1 positional sub-pattern ({pos_len} given)"
            ));
        }
        let mro = self.class_mro_slice(&target);
        let mut attr_names: Option<Vec<String>> = None;
        for cname in &mro {
            if let Some(cdef) = self.classes.get(cname) {
                if let Some(Val::List(items)) = cdef.attrs.get("__match_args__") {
                    let mut names = Vec::with_capacity(items.len());
                    for item in items {
                        let Val::Str(s) = item else {
                            return Err(format!("{target}.__match_args__ must contain strings"));
                        };
                        names.push(s.clone());
                    }
                    attr_names = Some(names);
                    break;
                }
            }
        }
        let attr_names = if let Some(names) = attr_names {
            names
        } else {
            let mut found = Vec::new();
            for cname in &mro {
                if let Some(cdef) = self.classes.get(cname) {
                    if let Some(def) = cdef
                        .methods
                        .get("init")
                        .or_else(|| cdef.methods.get("__init__"))
                    {
                        for (p, ty) in def.params.iter() {
                            if matches!(ty.as_deref(), Some("*") | Some("**")) {
                                break;
                            }
                            if p != "self" {
                                found.push(p.clone());
                            }
                        }
                        break;
                    }
                }
            }
            found
        };
        if pos_len > attr_names.len() {
            return Err(format!(
                "{target}() accepts {} positional sub-patterns ({pos_len} given)",
                attr_names.len()
            ));
        }
        let mut out = Vec::with_capacity(pos_len);
        for name in &attr_names[..pos_len] {
            match self.vm_get_attr(val, name) {
                Ok(v) => out.push(v),
                Err(_) => return Ok(Val::Null),
            }
        }
        Ok(Val::List(out))
    }
    pub(crate) fn vm_set_attr(
        &mut self,
        obj: &Val,
        name: String,
        value: Val,
    ) -> Result<(), String> {
        if crate::py_bridge::is_py_handle(obj) {
            let _ = crate::py_bridge::py_set(obj, &name, &value)?;
            return Ok(());
        }
        let Val::Object(id) = *obj else {
            return Err("member assignment requires object".into());
        };
        let snap = self.object_data_snapshot(id)?;
        if let Some(Val::Str(cls_name)) = snap.fields.get("__veyra_class__") {
            let is_fn = matches!(value, Val::Function(ref fk) if !fk.starts_with("__builtin__:"))
                .then(|| self.callable_def(&value).ok().map(|(_, d)| d))
                .flatten();
            let map = Arc::make_mut(&mut self.classes);
            let cls = map
                .get_mut(cls_name)
                .ok_or_else(|| format!("unknown class '{cls_name}'"))?;
            cls.attrs.remove(&name);
            cls.methods.remove(&name);
            if let Some(def) = is_fn {
                cls.methods.insert(name, def);
            } else {
                cls.attrs.insert(name, value);
            }
            self.method_cache.clear();
            return Ok(());
        }
        if name != "__setattr__" {
            if let Some((owner, def)) =
                self.find_method_with_owner_cached(&snap.class_name, "__setattr__")
            {
                if owner != "object" && owner != "type" {
                    let _ = self.call_def_kw_ctx(
                        def,
                        vec![Val::Object(id), Val::Str(name), value],
                        Vec::new(),
                        Some(owner),
                        Some(Val::Object(id)),
                    )?;
                    return Ok(());
                }
            }
        }
        let mro = self.class_mro_slice(&snap.class_name);
        if let Some((owner, true, attr_val, _)) = self.find_member_in_mro_slice(&mro, &name) {
            if let Val::Object(did) = attr_val {
                let dcls = self.object_class_name(did)?;
                if dcls == "__property__" {
                    let df = self.object_data_snapshot(did)?;
                    let fset = df.fields.get("fset").cloned().unwrap_or(Val::Null);
                    if matches!(fset, Val::Null) {
                        return Err(format!("property '{name}' has no setter"));
                    }
                    let _ = self.invoke_ctx_sync(
                        &fset,
                        vec![Val::Object(id), value],
                        Vec::new(),
                        Some(owner),
                        Some(Val::Object(id)),
                    )?;
                    return Ok(());
                }
                if let Some((_, set_def)) = self.find_method_with_owner_cached(&dcls, "__set__") {
                    let _ = self.call_def_kw_ctx(
                        set_def,
                        vec![attr_val, Val::Object(id), value],
                        Vec::new(),
                        Some(dcls),
                        Some(Val::Object(did)),
                    )?;
                    return Ok(());
                }
            }
        }
        let mut heap = self.objects.lock().map_err(|_| "object heap poisoned")?;
        let data = heap.get_mut(&id).ok_or("dangling object")?;
        data.fields.insert(name, value);
        Ok(())
    }
    pub(crate) fn vm_del_attr(&mut self, obj: &Val, name: &str) -> Result<(), String> {
        let Val::Object(id) = *obj else {
            return Err("delattr requires Object".into());
        };
        let snap = self.object_data_snapshot(id)?;
        if let Some(Val::Str(cls_name)) = snap.fields.get("__veyra_class__") {
            let map = Arc::make_mut(&mut self.classes);
            let cls = map
                .get_mut(cls_name)
                .ok_or_else(|| format!("unknown class '{cls_name}'"))?;
            let removed = cls.attrs.remove(name).is_some() || cls.methods.remove(name).is_some();
            self.method_cache.clear();
            if removed {
                return Ok(());
            }
            return Err(format!(
                "type object '{cls_name}' has no attribute '{name}'"
            ));
        }
        let mro = self.class_mro_slice(&snap.class_name);
        if let Some((owner, true, attr_val, _)) = self.find_member_in_mro_slice(&mro, name) {
            if let Val::Object(did) = attr_val {
                let dcls = self.object_class_name(did)?;
                if dcls == "__property__" {
                    let df = self.object_data_snapshot(did)?;
                    let fdel = df.fields.get("fdel").cloned().unwrap_or(Val::Null);
                    if matches!(fdel, Val::Null) {
                        return Err(format!("property '{name}' has no deleter"));
                    }
                    let _ = self.invoke_ctx_sync(
                        &fdel,
                        vec![Val::Object(id)],
                        Vec::new(),
                        Some(owner),
                        Some(Val::Object(id)),
                    )?;
                    return Ok(());
                }
                if let Some((_, del_def)) = self.find_method_with_owner_cached(&dcls, "__delete__")
                {
                    let _ = self.call_def_kw_ctx(
                        del_def,
                        vec![attr_val, Val::Object(id)],
                        Vec::new(),
                        Some(dcls),
                        Some(Val::Object(did)),
                    )?;
                    return Ok(());
                }
            }
        }
        if name != "__delattr__" {
            if let Some((owner, def)) =
                self.find_method_with_owner_cached(&snap.class_name, "__delattr__")
            {
                if owner != "object" && owner != "type" {
                    let _ = self.call_def_kw_ctx(
                        def,
                        vec![Val::Object(id), Val::Str(name.to_string())],
                        Vec::new(),
                        Some(owner),
                        Some(Val::Object(id)),
                    )?;
                    return Ok(());
                }
            }
        }
        let mut heap = self.objects.lock().map_err(|_| "object heap poisoned")?;
        let data = heap.get_mut(&id).ok_or("dangling object")?;
        data.fields
            .remove(name)
            .ok_or_else(|| format!("undefined field '{name}'"))?;
        Ok(())
    }
    fn invoke_val_in_execute_kw(
        &mut self,
        callee: Val,
        args: Vec<Val>,
        kwargs: Vec<(String, Val)>,
        active_class: Option<String>,
        active_self: Option<Val>,
    ) -> Result<(), String> {
        if crate::py_bridge::is_py_handle(&callee) {
            let res = if kwargs.is_empty() {
                crate::py_bridge::py_call(&callee, &args, None)?
            } else {
                let kw_map = Val::Map(kwargs.into_iter().map(|(k, v)| (Val::Str(k), v)).collect());
                crate::py_bridge::py_call(&callee, &args, Some(&kw_map))?
            };
            self.stack.push(res);
            return Ok(());
        }
        if let Val::Function(ref s) = callee {
            if let Some(bname) = s.strip_prefix("__builtin__:") {
                let res = self.call_builtin_wrapper_sync(bname, args, kwargs)?;
                self.stack.push(res);
                return Ok(());
            }
        }
        if let Val::Object(id) = callee {
            let snap = self.object_data_snapshot(id)?;
            if snap.class_name == "__bound_method__" {
                let func = snap
                    .fields
                    .get("__func__")
                    .cloned()
                    .ok_or("corrupt bound method")?;
                let bound_self = snap
                    .fields
                    .get("__self__")
                    .cloned()
                    .ok_or("corrupt bound method")?;
                let owner = snap.fields.get("__owner__").map(|v| v.show());
                let mut full_args = Vec::with_capacity(1 + args.len());
                full_args.push(bound_self.clone());
                full_args.extend(args);
                return self.invoke_val_in_execute_kw(
                    func,
                    full_args,
                    kwargs,
                    owner,
                    Some(bound_self),
                );
            }
            if let Some(Val::Str(cls_name)) = snap.fields.get("__veyra_class__") {
                let inst = self.instantiate_kw(cls_name, args, kwargs)?;
                self.stack.push(inst);
                return Ok(());
            }
            let (owner, def) = self
                .find_method_with_owner_cached(&snap.class_name, "__call__")
                .ok_or_else(|| format!("'{}' object is not callable", snap.class_name))?;
            let mut call_args = Vec::with_capacity(1 + args.len());
            call_args.push(Val::Object(id));
            call_args.extend(args);
            return self.invoke_in_execute_kw_ctx(
                def,
                call_args,
                kwargs,
                Some(owner),
                Some(Val::Object(id)),
            );
        }
        let (_, def) = self.callable_def(&callee)?;
        self.invoke_in_execute_kw_ctx(def, args, kwargs, active_class, active_self)
    }
    fn call_object_method_in_execute(
        &mut self,
        id: u64,
        name: &str,
        args: Vec<Val>,
        kwargs: Vec<(String, Val)>,
    ) -> Result<(), String> {
        let snap = self.object_data_snapshot(id)?;
        if snap.class_name != "__super__"
            && snap.class_name != "__property__"
            && snap.class_name != "__bound_method__"
            && !snap.fields.contains_key("__veyra_class__")
            && !snap.fields.contains_key(name)
        {
            let mro = self.class_mro_slice(&snap.class_name);
            if let Some((owner, false, _, Some(def))) = self.find_member_in_mro_slice(&mro, name) {
                let mut call_args = Vec::with_capacity(1 + args.len());
                call_args.push(Val::Object(id));
                call_args.extend(args);
                return self.invoke_in_execute_kw_ctx(
                    def,
                    call_args,
                    kwargs,
                    Some(owner),
                    Some(Val::Object(id)),
                );
            }
        }
        let member_val = self.vm_get_attr(&Val::Object(id), name)?;
        self.invoke_val_in_execute_kw(member_val, args, kwargs, None, None)
    }
    fn build_class_op(
        &mut self,
        name: String,
        base_count: usize,
        body_code: Vec<Op>,
    ) -> Result<(), String> {
        self.ensure_builtin_classes();
        let meta_val = self.pop()?;
        let mut raw_bases = Vec::with_capacity(base_count);
        for _ in 0..base_count {
            raw_bases.push(self.pop()?);
        }
        raw_bases.reverse();
        let mut bases = Vec::with_capacity(raw_bases.len());
        for b in &raw_bases {
            bases.push(self.resolve_class_name_arg(b)?);
        }
        if bases.is_empty() && name != "object" {
            bases.push("object".to_string());
        }
        let mut child = Vm::new(body_code);
        child.vars = self.vars.clone();
        child.cells = self.cells.clone();
        child.funcs = self.funcs.clone();
        child.classes = self.classes.clone();
        child.objects = self.objects.clone();
        child.class_objects = self.class_objects.clone();
        child.gc = self.gc.clone();
        child.debugger = self.debugger.clone();
        let ns_val = child.execute()?;
        for (k, v) in child.funcs.iter() {
            if !self.funcs.contains_key(k) {
                Arc::make_mut(&mut self.funcs).insert(k.clone(), v.clone());
            }
        }
        for (k, v) in child.classes.iter() {
            if !self.classes.contains_key(k) {
                Arc::make_mut(&mut self.classes).insert(k.clone(), v.clone());
            }
        }
        self.method_cache.clear();
        let Val::Map(ns_pairs) = ns_val else {
            return Err("internal error: class body did not return namespace map".into());
        };

        let meta_name = if !matches!(meta_val, Val::Null) {
            if let Ok(mname) = self.resolve_class_name_arg(&meta_val) {
                mname
            } else {
                let mut base_objs = Vec::with_capacity(bases.len());
                for b in &bases {
                    base_objs.push(self.class_object_val(b)?);
                }
                let res = self.call_val_kw_sync(
                    &meta_val,
                    vec![Val::Str(name), Val::List(base_objs), Val::Map(ns_pairs)],
                    Vec::new(),
                )?;
                self.stack.push(res);
                return Ok(());
            }
        } else {
            let mut chosen = "type".to_string();
            for b in &bases {
                if let Some(bm) = self.classes.get(b).and_then(|c| c.metaclass.clone()) {
                    if self.class_is_a(&bm, &chosen) {
                        chosen = bm;
                    } else if !self.class_is_a(&chosen, &bm) {
                        return Err("metaclass conflict among class bases".into());
                    }
                }
            }
            chosen
        };

        if meta_name == "type" {
            let cls_obj = self.register_class_from_ns("type", &name, &bases, ns_pairs)?;
            self.stack.push(cls_obj);
            return Ok(());
        }

        let meta_cls_obj = self.class_object_val(&meta_name)?;
        let mut base_objs = Vec::with_capacity(bases.len());
        for b in &bases {
            base_objs.push(self.class_object_val(b)?);
        }
        let bases_list = Val::List(base_objs);
        let ns_map_val = Val::Map(ns_pairs);

        let custom_new = self
            .find_method_with_owner_cached(&meta_name, "__new__")
            .filter(|(o, _)| o != "type" && o != "object");
        let cls_obj = if let Some((new_owner, new_def)) = custom_new {
            self.call_def_kw_ctx(
                new_def,
                vec![
                    meta_cls_obj.clone(),
                    Val::Str(name.clone()),
                    bases_list.clone(),
                    ns_map_val.clone(),
                ],
                Vec::new(),
                Some(new_owner),
                Some(meta_cls_obj),
            )?
        } else {
            let Val::Map(ns_pairs_owned) = ns_map_val.clone() else {
                unreachable!();
            };
            self.register_class_from_ns(&meta_name, &name, &bases, ns_pairs_owned)?
        };

        if let Some(created_name) = self.class_object_tag(&cls_obj)? {
            let actual_meta = self
                .classes
                .get(&created_name)
                .and_then(|c| c.metaclass.clone())
                .unwrap_or_else(|| meta_name.clone());
            if let Some((init_owner, init_def)) = self
                .find_method_with_owner_cached(&actual_meta, "__init__")
                .filter(|(o, _)| o != "type" && o != "object")
            {
                let _ = self.call_def_kw_ctx(
                    init_def,
                    vec![cls_obj.clone(), Val::Str(name), bases_list, ns_map_val],
                    Vec::new(),
                    Some(init_owner),
                    Some(cls_obj.clone()),
                )?;
            }
        }
        self.stack.push(cls_obj);
        Ok(())
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
            "property" => {
                let mut fget = pos.first().cloned().unwrap_or(Val::Null);
                let mut fset = pos.get(1).cloned().unwrap_or(Val::Null);
                let mut fdel = pos.get(2).cloned().unwrap_or(Val::Null);
                if pos.len() > 3 {
                    return Err(
                        "property expects at most 3 positional arguments (fget, fset, fdel)".into(),
                    );
                }
                if let Some(v) = Self::pop_kw(&mut kwargs, "fget") {
                    fget = v;
                }
                if let Some(v) = Self::pop_kw(&mut kwargs, "fset") {
                    fset = v;
                }
                if let Some(v) = Self::pop_kw(&mut kwargs, "fdel") {
                    fdel = v;
                }
                if let Some((bad, _)) = kwargs.first() {
                    return Err(format!("property got unexpected keyword argument '{bad}'"));
                }
                self.make_property_obj(fget, fset, fdel)?
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

    fn collect_val_gc_refs(
        val: &Val,
        visited: &mut HashSet<u64>,
        work_objs: &mut Vec<u64>,
        seen_funcs: &mut HashSet<String>,
        pending_funcs: &mut Vec<String>,
        seen_gens: &mut HashSet<u64>,
        pending_gens: &mut Vec<u64>,
    ) {
        let mut stack = vec![val];
        while let Some(cur) = stack.pop() {
            match cur {
                Val::Object(id) => {
                    if visited.insert(*id) {
                        work_objs.push(*id);
                    }
                }
                Val::List(xs) => {
                    for x in xs {
                        stack.push(x);
                    }
                }
                Val::Map(pairs) => {
                    for (k, v) in pairs {
                        stack.push(k);
                        stack.push(v);
                    }
                }
                Val::Function(name) => {
                    if seen_funcs.insert(name.clone()) {
                        pending_funcs.push(name.clone());
                    }
                }
                Val::Generator(gid) => {
                    if seen_gens.insert(*gid) {
                        pending_gens.push(*gid);
                    }
                }
                Val::Num(_) | Val::Str(_) | Val::Bool(_) | Val::Null => {}
            }
        }
    }

    fn seed_vm_gc_roots(
        vm: &Vm,
        visited: &mut HashSet<u64>,
        work_objs: &mut Vec<u64>,
        seen_funcs: &mut HashSet<String>,
        pending_funcs: &mut Vec<String>,
        seen_gens: &mut HashSet<u64>,
        pending_gens: &mut Vec<u64>,
    ) {
        let mut push = |v: &Val| {
            Self::collect_val_gc_refs(
                v,
                visited,
                work_objs,
                seen_funcs,
                pending_funcs,
                seen_gens,
                pending_gens,
            );
        };
        for v in vm.vars.values() {
            push(v);
        }
        for v in &vm.locals {
            push(v);
        }
        for cell in vm.cells.values() {
            if let Ok(guard) = cell.lock() {
                push(&guard);
            }
        }
        for v in &vm.stack {
            push(v);
        }
        if let Some(ref s) = vm.active_self {
            push(s);
        }
        for frame in &vm.frames {
            for v in frame.vars.values() {
                push(v);
            }
            for v in &frame.locals {
                push(v);
            }
            for cell in frame.cells.values() {
                if let Ok(guard) = cell.lock() {
                    push(&guard);
                }
            }
            if let Some(ref s) = frame.active_self {
                push(s);
            }
        }
        for cls in vm.classes.values() {
            for v in cls.attrs.values() {
                push(v);
            }
            for m in cls.methods.values() {
                for v in m.captures.values() {
                    push(v);
                }
                for cell in m.upvalues.values() {
                    if let Ok(guard) = cell.lock() {
                        push(&guard);
                    }
                }
            }
        }
        for (fname, fdef) in vm.funcs.iter() {
            // Anonymous closures start with "__" and remain alive only while reachable.
            if !fname.starts_with("__") {
                for v in fdef.captures.values() {
                    push(v);
                }
                for cell in fdef.upvalues.values() {
                    if let Ok(guard) = cell.lock() {
                        push(&guard);
                    }
                }
            }
        }
    }

    fn run_mark_sweep_objects(&self) -> Result<(usize, usize), String> {
        let mut visited: HashSet<u64> = HashSet::new();
        let mut work_objs: Vec<u64> = Vec::new();
        let mut seen_funcs: HashSet<String> = HashSet::new();
        let mut pending_funcs: Vec<String> = Vec::new();
        let mut seen_gens: HashSet<u64> = HashSet::new();
        let mut pending_gens: Vec<u64> = Vec::new();

        Self::seed_vm_gc_roots(
            self,
            &mut visited,
            &mut work_objs,
            &mut seen_funcs,
            &mut pending_funcs,
            &mut seen_gens,
            &mut pending_gens,
        );
        if let Ok(class_objs) = self.class_objects.lock() {
            for &id in class_objs.values() {
                if visited.insert(id) {
                    work_objs.push(id);
                }
            }
        }

        while !work_objs.is_empty() || !pending_funcs.is_empty() || !pending_gens.is_empty() {
            while let Some(fname) = pending_funcs.pop() {
                let def_opt = self
                    .funcs
                    .get(&fname)
                    .cloned()
                    .or_else(|| resolve_function_object(&fname).ok().flatten());
                if let Some(def) = def_opt {
                    for v in def.captures.values() {
                        Self::collect_val_gc_refs(
                            v,
                            &mut visited,
                            &mut work_objs,
                            &mut seen_funcs,
                            &mut pending_funcs,
                            &mut seen_gens,
                            &mut pending_gens,
                        );
                    }
                    for cell in def.upvalues.values() {
                        if let Ok(guard) = cell.lock() {
                            Self::collect_val_gc_refs(
                                &guard,
                                &mut visited,
                                &mut work_objs,
                                &mut seen_funcs,
                                &mut pending_funcs,
                                &mut seen_gens,
                                &mut pending_gens,
                            );
                        }
                    }
                }
            }
            while let Some(gid) = pending_gens.pop() {
                if let Ok(gens) = generators().lock()
                    && let Some(gvm) = gens.get(&gid)
                    && Arc::ptr_eq(&gvm.objects, &self.objects)
                {
                    Self::seed_vm_gc_roots(
                        gvm,
                        &mut visited,
                        &mut work_objs,
                        &mut seen_funcs,
                        &mut pending_funcs,
                        &mut seen_gens,
                        &mut pending_gens,
                    );
                }
            }
            if !work_objs.is_empty() {
                let batch = std::mem::take(&mut work_objs);
                let field_vals: Vec<Val> = {
                    let heap = self.objects.lock().map_err(|_| "object heap poisoned")?;
                    let mut out = Vec::new();
                    for oid in batch {
                        if let Some(obj) = heap.get(&oid) {
                            out.extend(obj.fields.values().cloned());
                        }
                    }
                    out
                };
                for fv in &field_vals {
                    Self::collect_val_gc_refs(
                        fv,
                        &mut visited,
                        &mut work_objs,
                        &mut seen_funcs,
                        &mut pending_funcs,
                        &mut seen_gens,
                        &mut pending_gens,
                    );
                }
            }
        }

        let (collected, live) = {
            let mut heap = self.objects.lock().map_err(|_| "object heap poisoned")?;
            let before = heap.len();
            heap.retain(|id, _| visited.contains(id));
            let after = heap.len();
            (before.saturating_sub(after), after)
        };

        self.gc.allocs_since_gc.store(0, Ordering::Relaxed);
        if let Ok(mut st) = self.gc.inner.lock() {
            st.collections += 1;
            st.last_collected = collected;
            st.total_collected += collected;
            st.next_threshold = (live.saturating_mul(2)).max(4096);
        }
        Ok((collected, live))
    }

    pub(crate) fn maybe_auto_gc(&self) -> Result<(), String> {
        if self.gc.paused.load(Ordering::Relaxed) {
            return Ok(());
        }
        let allocs = self.gc.allocs_since_gc.load(Ordering::Relaxed);
        if allocs < 4096 {
            return Ok(());
        }
        let threshold = self
            .gc
            .inner
            .lock()
            .map(|s| s.next_threshold)
            .unwrap_or(4096);
        let heap_len = self.objects.lock().map(|h| h.len()).unwrap_or(0);
        if heap_len >= threshold || allocs >= threshold {
            let _ = self.run_mark_sweep_objects()?;
            let _ = self.gc_tensors()?;
        }
        Ok(())
    }

    pub(crate) fn gc_collect(&self) -> Result<Val, String> {
        let (collected, live) = self.run_mark_sweep_objects()?;
        let tensors_collected = self.gc_tensors()?;
        let (collections, total_collected) = self
            .gc
            .inner
            .lock()
            .map(|s| (s.collections, s.total_collected))
            .unwrap_or((1, collected));
        let paused = self.gc.paused.load(Ordering::Relaxed);
        Ok(Val::Map(vec![
            (Val::Str("collected".into()), Val::Num(collected as f64)),
            (Val::Str("live_objects".into()), Val::Num(live as f64)),
            (
                Val::Str("tensors_collected".into()),
                Val::Num(tensors_collected as f64),
            ),
            (Val::Str("collections".into()), Val::Num(collections as f64)),
            (
                Val::Str("total_collected".into()),
                Val::Num(total_collected as f64),
            ),
            (Val::Str("paused".into()), Val::Bool(paused)),
        ]))
    }

    pub(crate) fn gc_stats(&self) -> Result<Val, String> {
        let live = self
            .objects
            .lock()
            .map_err(|_| "object heap poisoned")?
            .len();
        let paused = self.gc.paused.load(Ordering::Relaxed);
        let st = self.gc.inner.lock().map_err(|_| "gc state poisoned")?;
        Ok(Val::Map(vec![
            (Val::Str("live_objects".into()), Val::Num(live as f64)),
            (
                Val::Str("collections".into()),
                Val::Num(st.collections as f64),
            ),
            (
                Val::Str("total_collected".into()),
                Val::Num(st.total_collected as f64),
            ),
            (
                Val::Str("last_collected".into()),
                Val::Num(st.last_collected as f64),
            ),
            (Val::Str("paused".into()), Val::Bool(paused)),
        ]))
    }

    pub(crate) fn gc_set_paused(&self, paused: bool) -> Val {
        self.gc.paused.store(paused, Ordering::Relaxed);
        Val::Bool(true)
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
                Ok(None) => {
                    self.maybe_auto_gc()?;
                }
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
                } else if self.classes.contains_key(&n) {
                    self.class_object_val(&n)?
                } else if matches!(
                    n.as_str(),
                    "property"
                        | "staticmethod"
                        | "classmethod"
                        | "super"
                        | "isinstance"
                        | "is_instance"
                        | "issubclass"
                        | "mro"
                        | "getattr"
                        | "hasattr"
                        | "setattr"
                        | "delattr"
                        | "type_of"
                        | "callable"
                ) {
                    Val::Function(format!("__builtin__:{n}"))
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
                let yes = match (a, b) {
                    (Val::Num(x), Val::Num(y)) => match op {
                        Op::Lt => x < y,
                        Op::Le => x <= y,
                        Op::Gt => x > y,
                        _ => x >= y,
                    },
                    (Val::Str(x), Val::Str(y)) => match op {
                        Op::Lt => x < y,
                        Op::Le => x <= y,
                        Op::Gt => x > y,
                        _ => x >= y,
                    },
                    _ => return Err("comparison requires numbers".into()),
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
                let value = self.vm_get_attr(&obj, &name)?;
                self.stack.push(value);
            }
            Op::SetMember(name) => {
                let value = self.pop()?;
                let obj = self.pop()?;
                self.vm_set_attr(&obj, name, value)?;
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
                        self.call_object_method_in_execute(id, &name, args, Vec::new())?;
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
                            if matches!(member, Val::Object(_)) {
                                let m = member.clone();
                                self.invoke_val_in_execute_kw(m, args, Vec::new(), None, None)?;
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
                } else if let Some(val) = self.get_var(&n) {
                    self.invoke_val_in_execute_kw(val, a, Vec::new(), None, None)?;
                } else if self.classes.contains_key(&n) {
                    let v = self.instantiate(&n, a)?;
                    self.stack.push(v);
                } else {
                    let def = self
                        .funcs
                        .get(&n)
                        .cloned()
                        .ok_or(format!("unknown function or class '{n}'"))?;
                    self.invoke_in_execute(def, a)?;
                }
            }
            Op::CallExpr(k) => {
                let mut a = Vec::with_capacity(k);
                for _ in 0..k {
                    a.push(self.pop()?);
                }
                a.reverse();
                let callee = self.pop()?;
                self.invoke_val_in_execute_kw(callee, a, Vec::new(), None, None)?;
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
                        } else if let Some(val) = self.get_var(&name) {
                            self.invoke_val_in_execute_kw(val, pos_args, kwargs, None, None)?;
                        } else if self.classes.contains_key(&name) {
                            let v = self.instantiate_kw(&name, pos_args, kwargs)?;
                            self.stack.push(v);
                        } else {
                            let def = self
                                .funcs
                                .get(&name)
                                .cloned()
                                .ok_or(format!("unknown function or class '{name}'"))?;
                            self.invoke_in_execute_kw(def, pos_args, kwargs)?;
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
                            self.call_object_method_in_execute(id, &name, pos_args, kwargs)?;
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
                                } else if matches!(member, Val::Object(_)) {
                                    let m = member.clone();
                                    self.invoke_val_in_execute_kw(m, pos_args, kwargs, None, None)?;
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
                        self.invoke_val_in_execute_kw(callee, pos_args, kwargs, None, None)?;
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
            Op::BuildClass(name, base_count, body_code) => {
                self.build_class_op(name, base_count, body_code)?;
            }
            Op::DefClass(name, parent, methods) => {
                self.ensure_builtin_classes();
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
                let bases = match parent {
                    Some(p) => vec![p],
                    None => vec!["object".to_string()],
                };
                let mro = compute_c3_mro(&name, &bases, |b| {
                    self.classes
                        .get(b)
                        .map(|c| {
                            if c.mro.is_empty() {
                                vec![b.to_string(), "object".to_string()]
                            } else {
                                c.mro.clone()
                            }
                        })
                        .ok_or_else(|| format!("unknown base class '{b}'"))
                })?;
                Arc::make_mut(&mut self.classes).insert(
                    name.clone(),
                    ClassDef {
                        bases,
                        metaclass: None,
                        mro,
                        attrs: HashMap::new(),
                        methods: compiled,
                    },
                );
                let cls_obj = self.class_object_val(&name)?;
                self.set_var(name, cls_obj);
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
