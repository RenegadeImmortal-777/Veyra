// Veyra native JIT compiler: translates hot numeric & numeric-list bytecode
// functions (including inter-function calls and mutual recursion) into
// in-memory executable x86_64 or ARM64 (aarch64) machine code with W^X memory
// protection and clean interpreted VM fallback on unsupported ops or guards.

#![allow(clippy::too_many_arguments)]

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
};

use crate::bytecode::{FunctionDef, Op};
use crate::value::Val;

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct JitSlice {
    pub(crate) ptr: *mut f64,
    pub(crate) len: usize,
}

#[repr(C)]
pub(crate) struct JitCallCtx {
    pub(crate) lists_ptr: *const JitSlice,
    pub(crate) list_count: usize,
    pub(crate) oob: u8,
    pub(crate) dyn_lists_ptr: *mut Vec<Vec<f64>>,
    pub(crate) dyn_slices_ptr: *mut Vec<JitSlice>,
}

pub(crate) type JitEntry =
    unsafe extern "C" fn(args_ptr: *const f64, argc: usize, ctx: *mut JitCallCtx) -> f64;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum ParamKind {
    Num,
    NumList,
}

impl ParamKind {
    fn tag(self) -> char {
        match self {
            ParamKind::Num => 'N',
            ParamKind::NumList => 'L',
        }
    }
    fn to_abstract(self) -> AbstractType {
        match self {
            ParamKind::Num => AbstractType::Num,
            ParamKind::NumList => AbstractType::NumList,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum JitReturnKind {
    Num,
    Bool,
    NumList,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum AbstractType {
    Num,
    Bool,
    NumList,
}

impl AbstractType {
    fn matches_param(self) -> Option<ParamKind> {
        match self {
            AbstractType::Num | AbstractType::Bool => Some(ParamKind::Num),
            AbstractType::NumList => Some(ParamKind::NumList),
        }
    }
}

pub(crate) struct ExecMem {
    pub(crate) ptr: *mut u8,
    #[allow(dead_code)]
    size: usize,
}

// Safety: Executable code pages are read-execute (RX) immutable machine code.
unsafe impl Send for ExecMem {}
unsafe impl Sync for ExecMem {}

impl Drop for ExecMem {
    fn drop(&mut self) {
        #[cfg(all(any(target_arch = "x86_64", target_arch = "aarch64"), windows))]
        {
            const MEM_RELEASE: u32 = 0x0000_8000;
            unsafe extern "system" {
                fn VirtualFree(
                    lpAddress: *mut core::ffi::c_void,
                    dwSize: usize,
                    dwFreeType: u32,
                ) -> i32;
            }
            if !self.ptr.is_null() {
                unsafe {
                    let _ = VirtualFree(self.ptr as *mut core::ffi::c_void, 0, MEM_RELEASE);
                }
            }
        }
        #[cfg(all(any(target_arch = "x86_64", target_arch = "aarch64"), unix))]
        {
            unsafe extern "C" {
                fn munmap(addr: *mut core::ffi::c_void, len: usize) -> i32;
            }
            if !self.ptr.is_null() && self.size > 0 {
                unsafe {
                    let _ = munmap(self.ptr as *mut core::ffi::c_void, self.size);
                }
            }
        }
    }
}

pub(crate) struct CompiledJitFn {
    _mem: ExecMem,
    entry: JitEntry,
    param_kinds: Vec<ParamKind>,
    return_kind: JitReturnKind,
    code_bytes: usize,
}

// Safety: `entry` points into immutable RX memory owned by `_mem` and only reads args slice/ctx.
unsafe impl Send for CompiledJitFn {}
unsafe impl Sync for CompiledJitFn {}

impl CompiledJitFn {
    pub(crate) fn invoke(&self, args: &[Val]) -> Option<Val> {
        if args.len() != self.param_kinds.len() {
            return None;
        }
        let mut raw_args = [0.0f64; 16];
        let mut heap_args: Vec<f64> = Vec::new();
        let mut flat_lists: Vec<Vec<f64>> = Vec::new();

        let pack_arg = |val: &Val, kind: ParamKind, lists: &mut Vec<Vec<f64>>| -> Option<f64> {
            match (kind, val) {
                (ParamKind::Num, Val::Num(n)) => Some(*n),
                (ParamKind::NumList, Val::List(items)) => {
                    let mut buf = Vec::with_capacity(items.len());
                    for it in items {
                        match it {
                            Val::Num(n) => buf.push(*n),
                            _ => return None,
                        }
                    }
                    let idx = lists.len();
                    lists.push(buf);
                    Some(f64::from_bits(idx as u64))
                }
                _ => None,
            }
        };

        let arg_ptr = if args.len() <= raw_args.len() {
            for (i, (v, &kind)) in args.iter().zip(self.param_kinds.iter()).enumerate() {
                raw_args[i] = pack_arg(v, kind, &mut flat_lists)?;
            }
            raw_args.as_ptr()
        } else {
            heap_args.reserve(args.len());
            for (v, &kind) in args.iter().zip(self.param_kinds.iter()) {
                heap_args.push(pack_arg(v, kind, &mut flat_lists)?);
            }
            heap_args.as_ptr()
        };

        let mut slices: Vec<JitSlice> = flat_lists
            .iter_mut()
            .map(|v| JitSlice {
                ptr: v.as_mut_ptr(),
                len: v.len(),
            })
            .collect();
        let mut ctx = JitCallCtx {
            lists_ptr: if slices.is_empty() {
                core::ptr::null()
            } else {
                slices.as_ptr()
            },
            list_count: slices.len(),
            oob: 0,
            dyn_lists_ptr: &mut flat_lists as *mut Vec<Vec<f64>>,
            dyn_slices_ptr: &mut slices as *mut Vec<JitSlice>,
        };
        let out = unsafe { (self.entry)(arg_ptr, args.len(), &mut ctx) };
        if ctx.oob != 0 {
            return None;
        }
        Some(match self.return_kind {
            JitReturnKind::Num => Val::Num(out),
            JitReturnKind::Bool => Val::Bool(out != 0.0 && !out.is_nan()),
            JitReturnKind::NumList => {
                let idx = out.to_bits() as usize;
                let items = flat_lists.get(idx)?;
                Val::List(items.iter().copied().map(Val::Num).collect())
            }
        })
    }
}

#[derive(Clone)]
enum CachedJit {
    Compiled(Arc<CompiledJitFn>),
    Unsupported(String),
}

struct JitStats {
    compiled_count: AtomicUsize,
    rejected_count: AtomicUsize,
    native_calls: AtomicU64,
    fallback_calls: AtomicU64,
    code_bytes: AtomicUsize,
}

static JIT_ENABLED: AtomicBool = AtomicBool::new(true);
static JIT_CACHE: OnceLock<Mutex<HashMap<String, CachedJit>>> = OnceLock::new();
static JIT_FN_SLOTS: OnceLock<Mutex<HashMap<String, &'static AtomicUsize>>> = OnceLock::new();
static JIT_STATS: OnceLock<JitStats> = OnceLock::new();

fn jit_cache() -> &'static Mutex<HashMap<String, CachedJit>> {
    JIT_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn jit_fn_slots() -> &'static Mutex<HashMap<String, &'static AtomicUsize>> {
    JIT_FN_SLOTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn get_or_alloc_fn_slot(variant_key: &str) -> Result<&'static AtomicUsize, String> {
    let mut guard = jit_fn_slots().lock().map_err(|_| "jit slot map poisoned")?;
    if let Some(&slot) = guard.get(variant_key) {
        return Ok(slot);
    }
    let leaked: &'static AtomicUsize = Box::leak(Box::new(AtomicUsize::new(0)));
    guard.insert(variant_key.to_string(), leaked);
    Ok(leaked)
}

fn jit_stats_store() -> &'static JitStats {
    JIT_STATS.get_or_init(|| JitStats {
        compiled_count: AtomicUsize::new(0),
        rejected_count: AtomicUsize::new(0),
        native_calls: AtomicU64::new(0),
        fallback_calls: AtomicU64::new(0),
        code_bytes: AtomicUsize::new(0),
    })
}

pub(crate) fn jit_available() -> bool {
    cfg!(any(
        all(target_arch = "x86_64", any(windows, unix)),
        all(target_arch = "aarch64", any(windows, unix))
    ))
}

pub(crate) fn jit_is_enabled() -> bool {
    if !jit_available() {
        return false;
    }
    if let Ok(val) = std::env::var("VEYRA_JIT") {
        let lower = val.trim().to_ascii_lowercase();
        if matches!(lower.as_str(), "0" | "false" | "off" | "no") {
            return false;
        }
    }
    JIT_ENABLED.load(Ordering::Relaxed)
}

pub(crate) fn jit_set_enabled(enabled: bool) -> bool {
    JIT_ENABLED.store(enabled, Ordering::Relaxed);
    jit_is_enabled()
}

pub(crate) fn jit_warmup_threshold() -> u32 {
    if let Ok(v) = std::env::var("VEYRA_JIT_THRESHOLD") {
        if let Ok(n) = v.trim().parse::<u32>() {
            return n.max(1);
        }
    }
    8
}

fn code_fingerprint(def: &FunctionDef) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    let feed_bytes = |h: &mut u64, bs: &[u8]| {
        for &b in bs {
            *h ^= b as u64;
            *h = h.wrapping_mul(0x100_0000_01b3);
        }
    };
    feed_bytes(&mut h, &(def.params.len() as u64).to_le_bytes());
    for (p, ty) in def.params.iter() {
        feed_bytes(&mut h, p.as_bytes());
        if let Some(t) = ty {
            feed_bytes(&mut h, t.as_bytes());
        }
    }
    if let Some(rt) = &def.return_type {
        feed_bytes(&mut h, rt.as_bytes());
    }
    feed_bytes(&mut h, format!("{:?}", def.code).as_bytes());
    h
}

pub(crate) fn jit_variant_key(name: &str, def: &FunctionDef, kinds: &[ParamKind]) -> String {
    let sig: String = kinds.iter().map(|k| k.tag()).collect();
    format!(
        "{name}#{sig}@{:x}_{:016x}",
        Arc::as_ptr(&def.code) as usize,
        code_fingerprint(def)
    )
}

fn infer_param_kinds(def: &FunctionDef) -> Result<Vec<ParamKind>, String> {
    let mut kinds = Vec::with_capacity(def.params.len());
    for (_, ty) in def.params.iter() {
        match ty.as_deref() {
            None | Some("Any") | Some("Number") | Some("Float") | Some("Int") => {
                kinds.push(ParamKind::Num)
            }
            Some("List") => kinds.push(ParamKind::NumList),
            _ => return Err("non-numeric/list parameter type annotation".into()),
        }
    }

    // Infer untyped parameters that flow into `len(param)` or `param[idx]` as NumList.
    let mut origins: Vec<Option<usize>> = vec![None; def.local_count.max(def.params.len())];
    for (i, origin) in origins.iter_mut().enumerate().take(def.params.len()) {
        *origin = Some(i);
    }
    let mut sim_stack: Vec<Option<usize>> = Vec::new();
    for op in def.code.iter() {
        match op {
            Op::Const(_) | Op::FunctionRef(_) => sim_stack.push(None),
            Op::LoadLocal(s) => sim_stack.push(origins.get(*s).copied().flatten()),
            Op::DefineLocal(s, _, _) | Op::StoreLocal(s) => {
                let top = sim_stack.pop().flatten();
                if *s < origins.len() && top.is_some() {
                    origins[*s] = top;
                }
            }
            Op::Pop | Op::Neg | Op::Not | Op::JumpIfFalse(_) | Op::Return => {
                let _ = sim_stack.pop();
            }
            Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Div
            | Op::Mod
            | Op::Pow
            | Op::Eq
            | Op::Ne
            | Op::Lt
            | Op::Le
            | Op::Gt
            | Op::Ge => {
                let _ = sim_stack.pop();
                let _ = sim_stack.pop();
                sim_stack.push(None);
            }
            Op::Index => {
                let _idx = sim_stack.pop();
                let container = sim_stack.pop().flatten();
                if let Some(p) = container {
                    if p < kinds.len() && def.params[p].1.is_none() {
                        kinds[p] = ParamKind::NumList;
                    }
                }
                sim_stack.push(None);
            }
            Op::Call(fn_name, argc) => {
                let mut popped = Vec::with_capacity(*argc);
                for _ in 0..*argc {
                    popped.push(sim_stack.pop().flatten());
                }
                popped.reverse();
                if fn_name == "len" && *argc == 1 {
                    if let Some(p) = popped[0] {
                        if p < kinds.len()
                            && matches!(def.params[p].1.as_deref(), None | Some("Any"))
                        {
                            kinds[p] = ParamKind::NumList;
                        }
                    }
                }
                sim_stack.push(None);
            }
            Op::Jump(_) | Op::Halt => {}
            _ => break,
        }
    }
    Ok(kinds)
}

fn observe_runtime_param_kinds(def: &FunctionDef, args: &[Val]) -> Option<Vec<ParamKind>> {
    if args.len() != def.params.len() {
        return None;
    }
    let mut kinds = Vec::with_capacity(args.len());
    for ((_, ty), v) in def.params.iter().zip(args.iter()) {
        match v {
            Val::Num(n) => {
                match ty.as_deref() {
                    None | Some("Any") | Some("Number") | Some("Float") => {}
                    Some("Int") if n.fract() == 0.0 => {}
                    _ => return None,
                }
                kinds.push(ParamKind::Num);
            }
            Val::List(xs) => {
                if !matches!(ty.as_deref(), None | Some("Any") | Some("List")) {
                    return None;
                }
                if !xs.iter().all(|x| matches!(x, Val::Num(_))) {
                    return None;
                }
                kinds.push(ParamKind::NumList);
            }
            _ => return None,
        }
    }
    Some(kinds)
}

pub(crate) fn jit_try_compile(
    name: &str,
    def: &FunctionDef,
    callee_lookup: &dyn Fn(&str) -> Option<FunctionDef>,
) -> Result<Arc<CompiledJitFn>, String> {
    let kinds = infer_param_kinds(def)?;
    jit_try_compile_with_kinds(name, def, &kinds, callee_lookup)
}

fn jit_try_compile_with_kinds(
    name: &str,
    def: &FunctionDef,
    kinds: &[ParamKind],
    callee_lookup: &dyn Fn(&str) -> Option<FunctionDef>,
) -> Result<Arc<CompiledJitFn>, String> {
    let key = jit_variant_key(name, def, kinds);
    {
        let guard = jit_cache().lock().map_err(|_| "jit cache poisoned")?;
        if let Some(entry) = guard.get(&key) {
            return match entry {
                CachedJit::Compiled(f) => Ok(f.clone()),
                CachedJit::Unsupported(reason) => Err(reason.clone()),
            };
        }
    }

    let stats = jit_stats_store();
    let res = compile_group_impl(name, def, kinds, callee_lookup);
    match res {
        Ok(root) => Ok(root),
        Err(reason) => {
            stats.rejected_count.fetch_add(1, Ordering::Relaxed);
            if let Ok(mut guard) = jit_cache().lock() {
                guard.insert(key, CachedJit::Unsupported(reason.clone()));
            }
            Err(reason)
        }
    }
}

pub(crate) fn jit_try_run(
    name: &str,
    def: &FunctionDef,
    args: &[Val],
    call_count: u32,
    callee_lookup: &dyn Fn(&str) -> Option<FunctionDef>,
) -> Option<Val> {
    if !jit_is_enabled() {
        return None;
    }
    if def.is_async || def.is_generator || !def.captures.is_empty() || !def.upvalues.is_empty() {
        return None;
    }
    let kinds = observe_runtime_param_kinds(def, args)?;
    let key = jit_variant_key(name, def, &kinds);
    let compiled = {
        let guard = jit_cache().lock().ok()?;
        match guard.get(&key) {
            Some(CachedJit::Compiled(f)) => Some(f.clone()),
            Some(CachedJit::Unsupported(_)) => {
                jit_stats_store()
                    .fallback_calls
                    .fetch_add(1, Ordering::Relaxed);
                return None;
            }
            None => None,
        }
    };
    let compiled = if let Some(f) = compiled {
        f
    } else if call_count >= jit_warmup_threshold() {
        match jit_try_compile_with_kinds(name, def, &kinds, callee_lookup) {
            Ok(f) => f,
            Err(_) => {
                jit_stats_store()
                    .fallback_calls
                    .fetch_add(1, Ordering::Relaxed);
                return None;
            }
        }
    } else {
        return None;
    };
    let res = compiled.invoke(args);
    if res.is_some() {
        jit_stats_store()
            .native_calls
            .fetch_add(1, Ordering::Relaxed);
    } else {
        jit_stats_store()
            .fallback_calls
            .fetch_add(1, Ordering::Relaxed);
    }
    res
}

pub(crate) fn jit_info_val() -> Val {
    let stats = jit_stats_store();
    let backend = if cfg!(all(target_arch = "x86_64", any(windows, unix))) {
        "x86_64-sse2"
    } else if cfg!(all(target_arch = "aarch64", any(windows, unix))) {
        "aarch64-neon"
    } else {
        "vm-interpreter"
    };
    Val::Map(vec![
        (Val::Str("available".into()), Val::Bool(jit_available())),
        (Val::Str("enabled".into()), Val::Bool(jit_is_enabled())),
        (Val::Str("backend".into()), Val::Str(backend.into())),
        (
            Val::Str("arch".into()),
            Val::Str(std::env::consts::ARCH.into()),
        ),
        (Val::Str("os".into()), Val::Str(std::env::consts::OS.into())),
        (
            Val::Str("threshold".into()),
            Val::Num(jit_warmup_threshold() as f64),
        ),
        (
            Val::Str("compiled_functions".into()),
            Val::Num(stats.compiled_count.load(Ordering::Relaxed) as f64),
        ),
        (
            Val::Str("rejected_functions".into()),
            Val::Num(stats.rejected_count.load(Ordering::Relaxed) as f64),
        ),
        (
            Val::Str("native_calls".into()),
            Val::Num(stats.native_calls.load(Ordering::Relaxed) as f64),
        ),
        (
            Val::Str("fallback_calls".into()),
            Val::Num(stats.fallback_calls.load(Ordering::Relaxed) as f64),
        ),
        (
            Val::Str("code_bytes".into()),
            Val::Num(stats.code_bytes.load(Ordering::Relaxed) as f64),
        ),
    ])
}

#[derive(Clone, PartialEq, Eq)]
enum CallResolution {
    SelfCall,
    LenList,
    AllocList(usize),
    MathHelper(MathHelper),
    InterFn(String), // variant key into JIT_FN_SLOTS
}

struct VerifiedPlan {
    stacks_at_ip: Vec<Option<Vec<AbstractType>>>,
    call_resolutions: HashMap<usize, CallResolution>,
    max_stack: usize,
    return_kind: JitReturnKind,
}

struct PlannedUnit {
    variant_key: String,
    def: FunctionDef,
    param_kinds: Vec<ParamKind>,
    plan: VerifiedPlan,
}

fn type_compat_with_kind(ty: Option<&String>, kind: ParamKind) -> bool {
    match kind {
        ParamKind::Num => matches!(
            ty.map(String::as_str),
            None | Some("Any") | Some("Number") | Some("Float") | Some("Int")
        ),
        ParamKind::NumList => matches!(ty.map(String::as_str), None | Some("Any") | Some("List")),
    }
}

fn is_vm_builtin_shadow(name: &str) -> bool {
    matches!(
        name,
        "print"
            | "println"
            | "range"
            | "type"
            | "str"
            | "int"
            | "float"
            | "bool"
            | "len"
            | "push"
            | "pop"
            | "assert"
    )
}

fn expected_return_from_annotation(def: &FunctionDef) -> JitReturnKind {
    if def.return_type.as_deref() == Some("Bool") {
        return JitReturnKind::Bool;
    }
    if let Some(rt) = def.return_type.as_deref()
        && (rt == "List" || rt.starts_with("List<"))
    {
        return JitReturnKind::NumList;
    }
    if def.return_type.as_deref().is_some() && def.return_type.as_deref() != Some("Any") {
        return JitReturnKind::Num;
    }
    for w in def.code.windows(2) {
        if matches!(w[1], Op::Return)
            && matches!(
                w[0],
                Op::Const(Val::Bool(_))
                    | Op::Eq
                    | Op::Ne
                    | Op::Lt
                    | Op::Le
                    | Op::Gt
                    | Op::Ge
                    | Op::Not
            )
        {
            return JitReturnKind::Bool;
        }
    }
    JitReturnKind::Num
}

fn verify_and_collect_plans(
    name: &str,
    def: &FunctionDef,
    param_kinds: &[ParamKind],
    callee_lookup: &dyn Fn(&str) -> Option<FunctionDef>,
    planned: &mut HashMap<String, PlannedUnit>,
    active: &mut HashSet<String>,
    use_cache: bool,
) -> Result<JitReturnKind, String> {
    let vkey = jit_variant_key(name, def, param_kinds);
    if let Some(unit) = planned.get(&vkey) {
        return Ok(unit.plan.return_kind);
    }
    if active.contains(&vkey) {
        return Ok(expected_return_from_annotation(def));
    }
    if use_cache {
        let guard = jit_cache().lock().map_err(|_| "jit cache poisoned")?;
        if let Some(entry) = guard.get(&vkey) {
            return match entry {
                CachedJit::Compiled(f) => Ok(f.return_kind),
                CachedJit::Unsupported(reason) => Err(reason.clone()),
            };
        }
    }

    if def.is_async || def.is_generator {
        return Err("async/generator functions use VM runtime".into());
    }
    if !def.captures.is_empty() || !def.upvalues.is_empty() {
        return Err("closure upvalues require VM frame".into());
    }
    if param_kinds.len() != def.params.len() {
        return Err("arity mismatch during JIT verification".into());
    }
    for ((_, pty), &pk) in def.params.iter().zip(param_kinds.iter()) {
        if !type_compat_with_kind(pty.as_ref(), pk) {
            return Err("incompatible parameter type annotation".into());
        }
    }
    match def.return_type.as_deref() {
        None | Some("Any") | Some("Number") | Some("Float") | Some("Bool") | Some("List") => {}
        Some("Int") => {
            if def.code.iter().any(|op| matches!(op, Op::Div | Op::Pow)) {
                return Err("Int return check uses VM for div/pow".into());
            }
        }
        _ => return Err("non-numeric return type annotation".into()),
    }

    let code = &def.code;
    if code.is_empty() {
        return Err("empty function body".into());
    }

    active.insert(vkey.clone());

    let n = code.len();
    let mut stacks_at_ip: Vec<Option<Vec<AbstractType>>> = vec![None; n];
    let mut local_types: Vec<Option<AbstractType>> =
        vec![None; def.local_count.max(def.params.len())];
    for (slot, pk) in local_types.iter_mut().zip(param_kinds.iter()) {
        *slot = Some(pk.to_abstract());
    }
    let mut call_resolutions: HashMap<usize, CallResolution> = HashMap::new();
    let mut worklist: VecDeque<usize> = VecDeque::new();
    stacks_at_ip[0] = Some(Vec::new());
    worklist.push_back(0);
    let mut max_stack = 0usize;
    let mut observed_return: Option<JitReturnKind> = None;

    let push_edge = |target: usize,
                     stack: &Vec<AbstractType>,
                     slots: &mut [Option<Vec<AbstractType>>],
                     wl: &mut VecDeque<usize>|
     -> Result<(), String> {
        if target >= n {
            return Err("jump target out of bounds".into());
        }
        match &slots[target] {
            None => {
                slots[target] = Some(stack.clone());
                wl.push_back(target);
            }
            Some(existing) if existing != stack => {
                return Err("inconsistent stack shape at control-flow merge".into());
            }
            Some(_) => {}
        }
        Ok(())
    };

    let result = (|| -> Result<JitReturnKind, String> {
        while let Some(ip) = worklist.pop_front() {
            let mut stack = stacks_at_ip[ip].clone().ok_or("missing block stack")?;
            let mut cur = ip;
            loop {
                max_stack = max_stack.max(stack.len());
                let op = &code[cur];
                match op {
                    Op::Const(Val::Num(_)) => stack.push(AbstractType::Num),
                    Op::Const(Val::Bool(_)) => stack.push(AbstractType::Bool),
                    Op::Const(Val::Null) => {
                        if matches!(code.get(cur + 1), Some(Op::Return)) {
                            stack.push(AbstractType::Num);
                        } else {
                            return Err("unsupported constant value in JIT".into());
                        }
                    }
                    Op::LoadLocal(s) => {
                        if *s >= local_types.len() {
                            return Err("local slot out of bounds".into());
                        }
                        let ty = local_types[*s].unwrap_or(AbstractType::Num);
                        stack.push(ty);
                    }
                    Op::DefineLocal(s, _, ty) => {
                        let val_ty = stack.pop().ok_or("stack underflow")?;
                        let compat = match val_ty {
                            AbstractType::NumList => {
                                matches!(ty.as_deref(), None | Some("Any") | Some("List"))
                            }
                            AbstractType::Num | AbstractType::Bool => matches!(
                                ty.as_deref(),
                                None | Some("Any") | Some("Number") | Some("Int") | Some("Bool")
                            ),
                        };
                        if !compat {
                            return Err("incompatible local type annotation".into());
                        }
                        if *s >= local_types.len() {
                            return Err("local slot out of bounds".into());
                        }
                        if let Some(prev) = local_types[*s] {
                            if prev != val_ty {
                                return Err("local slot changes representation type".into());
                            }
                        } else {
                            local_types[*s] = Some(val_ty);
                        }
                    }
                    Op::StoreLocal(s) => {
                        let val_ty = stack.pop().ok_or("stack underflow")?;
                        if *s >= local_types.len() {
                            return Err("local slot out of bounds".into());
                        }
                        if let Some(prev) = local_types[*s] {
                            if prev != val_ty {
                                return Err("local slot changes representation type".into());
                            }
                        } else {
                            local_types[*s] = Some(val_ty);
                        }
                    }
                    Op::Pop => {
                        stack.pop().ok_or("stack underflow")?;
                    }
                    Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Mod | Op::Pow => {
                        let b = stack.pop().ok_or("stack underflow")?;
                        let a = stack.pop().ok_or("stack underflow")?;
                        if a != AbstractType::Num || b != AbstractType::Num {
                            return Err("non-numeric arithmetic operand".into());
                        }
                        stack.push(AbstractType::Num);
                    }
                    Op::Eq | Op::Ne => {
                        let b = stack.pop().ok_or("stack underflow")?;
                        let a = stack.pop().ok_or("stack underflow")?;
                        if a != b || a == AbstractType::NumList {
                            return Err("unsupported equality operand types in JIT".into());
                        }
                        stack.push(AbstractType::Bool);
                    }
                    Op::Lt | Op::Le | Op::Gt | Op::Ge => {
                        let b = stack.pop().ok_or("stack underflow")?;
                        let a = stack.pop().ok_or("stack underflow")?;
                        if a != AbstractType::Num || b != AbstractType::Num {
                            return Err("ordered comparison requires numbers".into());
                        }
                        stack.push(AbstractType::Bool);
                    }
                    Op::Neg => {
                        let a = stack.pop().ok_or("stack underflow")?;
                        if a != AbstractType::Num {
                            return Err("neg requires number".into());
                        }
                        stack.push(AbstractType::Num);
                    }
                    Op::Not => {
                        let a = stack.pop().ok_or("stack underflow")?;
                        if a == AbstractType::NumList {
                            return Err("not on list falls back to VM".into());
                        }
                        stack.push(AbstractType::Bool);
                    }
                    Op::Index => {
                        let idx = stack.pop().ok_or("stack underflow")?;
                        let container = stack.pop().ok_or("stack underflow")?;
                        if idx != AbstractType::Num || container != AbstractType::NumList {
                            return Err("JIT index requires (List, Number)".into());
                        }
                        stack.push(AbstractType::Num);
                    }
                    Op::SetIndex => {
                        let val = stack.pop().ok_or("stack underflow")?;
                        let idx = stack.pop().ok_or("stack underflow")?;
                        let container = stack.pop().ok_or("stack underflow")?;
                        if val != AbstractType::Num
                            || idx != AbstractType::Num
                            || container != AbstractType::NumList
                        {
                            return Err(
                                "JIT setindex requires (List<Number>, Number, Number)".into()
                            );
                        }
                        stack.push(AbstractType::NumList);
                    }
                    Op::Call(fn_name, argc) => {
                        if stack.len() < *argc {
                            return Err("call stack underflow".into());
                        }
                        let mut arg_types = Vec::with_capacity(*argc);
                        for _ in 0..*argc {
                            arg_types.push(stack.pop().ok_or("stack underflow")?);
                        }
                        arg_types.reverse();

                        let mut arg_kinds = Vec::with_capacity(*argc);
                        for ty in &arg_types {
                            arg_kinds
                                .push(ty.matches_param().ok_or("unsupported call argument type")?);
                        }

                        if fn_name == "len" && *argc == 1 && arg_types[0] == AbstractType::NumList {
                            call_resolutions.insert(cur, CallResolution::LenList);
                            stack.push(AbstractType::Num);
                        } else if fn_name == "__list"
                            && arg_types.iter().all(|t| *t == AbstractType::Num)
                        {
                            call_resolutions.insert(cur, CallResolution::AllocList(*argc));
                            stack.push(AbstractType::NumList);
                        } else if fn_name == name
                            && arg_kinds == param_kinds
                            && *argc == def.params.len()
                        {
                            call_resolutions.insert(cur, CallResolution::SelfCall);
                            let rk = expected_return_from_annotation(def);
                            stack.push(match rk {
                                JitReturnKind::Bool => AbstractType::Bool,
                                JitReturnKind::Num => AbstractType::Num,
                                JitReturnKind::NumList => AbstractType::NumList,
                            });
                        } else if let Some(mh) = math_helper_kind(fn_name, *argc) {
                            if !arg_types.iter().all(|t| *t == AbstractType::Num) {
                                return Err("math helper requires numeric args".into());
                            }
                            call_resolutions.insert(cur, CallResolution::MathHelper(mh));
                            stack.push(AbstractType::Num);
                        } else if !is_vm_builtin_shadow(fn_name)
                            && let Some(callee_def) = callee_lookup(fn_name)
                            && callee_def.params.len() == *argc
                        {
                            let callee_vkey = jit_variant_key(fn_name, &callee_def, &arg_kinds);
                            let callee_ret = verify_and_collect_plans(
                                fn_name,
                                &callee_def,
                                &arg_kinds,
                                callee_lookup,
                                planned,
                                active,
                                use_cache,
                            )?;
                            call_resolutions.insert(cur, CallResolution::InterFn(callee_vkey));
                            stack.push(match callee_ret {
                                JitReturnKind::Bool => AbstractType::Bool,
                                JitReturnKind::Num => AbstractType::Num,
                                JitReturnKind::NumList => AbstractType::NumList,
                            });
                        } else {
                            return Err(format!("unsupported JIT call '{fn_name}/{argc}'"));
                        }
                    }
                    Op::Jump(target) => {
                        push_edge(*target, &stack, &mut stacks_at_ip, &mut worklist)?;
                        break;
                    }
                    Op::JumpIfFalse(target) => {
                        let cond = stack.pop().ok_or("stack underflow")?;
                        if cond == AbstractType::NumList {
                            return Err("list truthiness falls back to VM".into());
                        }
                        push_edge(*target, &stack, &mut stacks_at_ip, &mut worklist)?;
                        push_edge(cur + 1, &stack, &mut stacks_at_ip, &mut worklist)?;
                        break;
                    }
                    Op::Return => {
                        let r = stack.pop().ok_or("return stack underflow")?;
                        let rk = match r {
                            AbstractType::Num => JitReturnKind::Num,
                            AbstractType::Bool => JitReturnKind::Bool,
                            AbstractType::NumList => JitReturnKind::NumList,
                        };
                        if let Some(prev) = observed_return {
                            if prev != rk {
                                return Err("mixed return value representations".into());
                            }
                        } else {
                            observed_return = Some(rk);
                        }
                        break;
                    }
                    Op::Halt => {
                        if let Some(prev) = observed_return {
                            if prev != JitReturnKind::Num {
                                return Err("mixed halt return representation".into());
                            }
                        } else {
                            observed_return = Some(JitReturnKind::Num);
                        }
                        break;
                    }
                    other => return Err(format!("unsupported bytecode op {:?}", other)),
                }
                cur += 1;
                if cur >= n {
                    return Err("bytecode fell off end without halt/return".into());
                }
                if let Some(existing) = &stacks_at_ip[cur] {
                    if existing != &stack {
                        return Err("inconsistent fallthrough stack depth".into());
                    }
                    break;
                }
                stacks_at_ip[cur] = Some(stack.clone());
            }
        }

        let return_kind = observed_return.unwrap_or(JitReturnKind::Num);
        if def.return_type.as_deref() == Some("Bool") && return_kind != JitReturnKind::Bool {
            return Err("expected Bool return".into());
        }
        if def.return_type.as_deref() == Some("List") && return_kind != JitReturnKind::NumList {
            return Err("expected List return".into());
        }
        Ok(return_kind)
    })();

    active.remove(&vkey);
    let return_kind = result?;
    planned.insert(
        vkey.clone(),
        PlannedUnit {
            variant_key: vkey,
            def: def.clone(),
            param_kinds: param_kinds.to_vec(),
            plan: VerifiedPlan {
                stacks_at_ip,
                call_resolutions,
                max_stack: max_stack + 4,
                return_kind,
            },
        },
    );
    Ok(return_kind)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MathHelper {
    One(usize),
    Two(usize),
}

unsafe extern "C" fn jit_abs(x: f64) -> f64 {
    x.abs()
}
unsafe extern "C" fn jit_sqrt(x: f64) -> f64 {
    x.sqrt()
}
unsafe extern "C" fn jit_sin(x: f64) -> f64 {
    x.sin()
}
unsafe extern "C" fn jit_cos(x: f64) -> f64 {
    x.cos()
}
unsafe extern "C" fn jit_tan(x: f64) -> f64 {
    x.tan()
}
unsafe extern "C" fn jit_exp(x: f64) -> f64 {
    x.exp()
}
unsafe extern "C" fn jit_log(x: f64) -> f64 {
    x.ln()
}
unsafe extern "C" fn jit_log10(x: f64) -> f64 {
    x.log10()
}
unsafe extern "C" fn jit_log2(x: f64) -> f64 {
    x.log2()
}
unsafe extern "C" fn jit_floor(x: f64) -> f64 {
    x.floor()
}
unsafe extern "C" fn jit_ceil(x: f64) -> f64 {
    x.ceil()
}
unsafe extern "C" fn jit_round(x: f64) -> f64 {
    x.round()
}
unsafe extern "C" fn jit_pow(a: f64, b: f64) -> f64 {
    a.powf(b)
}
unsafe extern "C" fn jit_fmod(a: f64, b: f64) -> f64 {
    a % b
}
unsafe extern "C" fn jit_hypot(a: f64, b: f64) -> f64 {
    a.hypot(b)
}
unsafe extern "C" fn jit_atan2(y: f64, x: f64) -> f64 {
    y.atan2(x)
}
unsafe extern "C" fn jit_min(a: f64, b: f64) -> f64 {
    a.min(b)
}
unsafe extern "C" fn jit_max(a: f64, b: f64) -> f64 {
    a.max(b)
}

unsafe extern "C" fn jit_list_len(ctx: *const JitCallCtx, list_idx: usize) -> f64 {
    if ctx.is_null() {
        return 0.0;
    }
    let ctx_ref = unsafe { &*ctx };
    if list_idx >= ctx_ref.list_count {
        return 0.0;
    }
    let slice = unsafe { &*ctx_ref.lists_ptr.add(list_idx) };
    slice.len as f64
}

unsafe extern "C" fn jit_list_get(ctx: *mut JitCallCtx, list_idx: usize, index_f64: f64) -> f64 {
    if ctx.is_null() {
        return 0.0;
    }
    let ctx_ref = unsafe { &mut *ctx };
    if list_idx >= ctx_ref.list_count {
        ctx_ref.oob = 1;
        return 0.0;
    }
    let slice = unsafe { &*ctx_ref.lists_ptr.add(list_idx) };
    if slice.len == 0 || !index_f64.is_finite() {
        ctx_ref.oob = 1;
        return 0.0;
    }
    let raw = index_f64 as isize;
    let len = slice.len as isize;
    let resolved = if raw < 0 { len + raw } else { raw };
    if resolved < 0 || (resolved as usize) >= slice.len {
        ctx_ref.oob = 1;
        return 0.0;
    }
    unsafe { *slice.ptr.add(resolved as usize) }
}

#[allow(dead_code)]
unsafe extern "C" fn jit_list_set(
    ctx: *mut JitCallCtx,
    list_idx: usize,
    index_f64: f64,
    value: f64,
) -> f64 {
    if ctx.is_null() {
        return 0.0;
    }
    let ctx_ref = unsafe { &mut *ctx };
    if list_idx >= ctx_ref.list_count {
        ctx_ref.oob = 1;
        return 0.0;
    }
    let slice = unsafe { &*ctx_ref.lists_ptr.add(list_idx) };
    if slice.len == 0 || !index_f64.is_finite() {
        ctx_ref.oob = 1;
        return 0.0;
    }
    let raw = index_f64 as isize;
    let len = slice.len as isize;
    let resolved = if raw < 0 { len + raw } else { raw };
    if resolved < 0 || (resolved as usize) >= slice.len {
        ctx_ref.oob = 1;
        return 0.0;
    }
    unsafe {
        *slice.ptr.add(resolved as usize) = value;
    }
    f64::from_bits(list_idx as u64)
}

unsafe extern "C" fn jit_alloc_list(
    elems_ptr: *const f64,
    len: usize,
    ctx: *mut JitCallCtx,
) -> f64 {
    if ctx.is_null() {
        return 0.0;
    }
    let ctx_ref = unsafe { &mut *ctx };
    if ctx_ref.dyn_lists_ptr.is_null() || ctx_ref.dyn_slices_ptr.is_null() {
        ctx_ref.oob = 1;
        return 0.0;
    }
    let lists = unsafe { &mut *ctx_ref.dyn_lists_ptr };
    let slices = unsafe { &mut *ctx_ref.dyn_slices_ptr };
    if lists.len() >= 1_000_000 {
        ctx_ref.oob = 1;
        return 0.0;
    }
    let mut vec = if len == 0 || elems_ptr.is_null() {
        Vec::new()
    } else {
        unsafe { core::slice::from_raw_parts(elems_ptr, len) }.to_vec()
    };
    let data_ptr = if len == 0 {
        core::ptr::null_mut()
    } else {
        vec.as_mut_ptr()
    };
    let idx = lists.len();
    lists.push(vec);
    slices.push(JitSlice { ptr: data_ptr, len });
    ctx_ref.lists_ptr = slices.as_ptr();
    ctx_ref.list_count = slices.len();
    f64::from_bits(idx as u64)
}

fn math_helper_kind(name: &str, argc: usize) -> Option<MathHelper> {
    match (name, argc) {
        ("abs", 1) => Some(MathHelper::One(jit_abs as *const () as usize)),
        ("sqrt", 1) => Some(MathHelper::One(jit_sqrt as *const () as usize)),
        ("sin", 1) => Some(MathHelper::One(jit_sin as *const () as usize)),
        ("cos", 1) => Some(MathHelper::One(jit_cos as *const () as usize)),
        ("tan", 1) => Some(MathHelper::One(jit_tan as *const () as usize)),
        ("exp", 1) => Some(MathHelper::One(jit_exp as *const () as usize)),
        ("log" | "ln", 1) => Some(MathHelper::One(jit_log as *const () as usize)),
        ("log10", 1) => Some(MathHelper::One(jit_log10 as *const () as usize)),
        ("log2", 1) => Some(MathHelper::One(jit_log2 as *const () as usize)),
        ("floor", 1) => Some(MathHelper::One(jit_floor as *const () as usize)),
        ("ceil", 1) => Some(MathHelper::One(jit_ceil as *const () as usize)),
        ("round", 1) => Some(MathHelper::One(jit_round as *const () as usize)),
        ("pow", 2) => Some(MathHelper::Two(jit_pow as *const () as usize)),
        ("min", 2) => Some(MathHelper::Two(jit_min as *const () as usize)),
        ("max", 2) => Some(MathHelper::Two(jit_max as *const () as usize)),
        ("fmod", 2) => Some(MathHelper::Two(jit_fmod as *const () as usize)),
        ("hypot", 2) => Some(MathHelper::Two(jit_hypot as *const () as usize)),
        ("atan2", 2) => Some(MathHelper::Two(jit_atan2 as *const () as usize)),
        _ => None,
    }
}

fn compile_group_impl(
    name: &str,
    def: &FunctionDef,
    param_kinds: &[ParamKind],
    callee_lookup: &dyn Fn(&str) -> Option<FunctionDef>,
) -> Result<Arc<CompiledJitFn>, String> {
    if !jit_available() {
        return Err("native JIT requires x86_64 or aarch64 host".into());
    }
    let root_key = jit_variant_key(name, def, param_kinds);
    let mut planned: HashMap<String, PlannedUnit> = HashMap::new();
    let mut active: HashSet<String> = HashSet::new();
    let _ = verify_and_collect_plans(
        name,
        def,
        param_kinds,
        callee_lookup,
        &mut planned,
        &mut active,
        true,
    )?;

    let mut slot_ptrs: HashMap<String, usize> = HashMap::new();
    let mut slot_atomics: HashMap<String, &'static AtomicUsize> = HashMap::new();
    for unit in planned.values() {
        for res in unit.plan.call_resolutions.values() {
            if let CallResolution::InterFn(callee_key) = res {
                if !slot_ptrs.contains_key(callee_key) {
                    let atom = get_or_alloc_fn_slot(callee_key)?;
                    slot_ptrs.insert(callee_key.clone(), atom as *const AtomicUsize as usize);
                    slot_atomics.insert(callee_key.clone(), atom);
                }
            }
        }
        if !slot_ptrs.contains_key(&unit.variant_key) {
            let atom = get_or_alloc_fn_slot(&unit.variant_key)?;
            slot_ptrs.insert(
                unit.variant_key.clone(),
                atom as *const AtomicUsize as usize,
            );
            slot_atomics.insert(unit.variant_key.clone(), atom);
        }
    }

    let stats = jit_stats_store();
    let mut compiled_units: Vec<(String, Arc<CompiledJitFn>)> = Vec::with_capacity(planned.len());
    for unit in planned.values() {
        let code = if cfg!(target_arch = "aarch64") {
            compile_aarch64_code(&unit.def, &unit.plan, &slot_ptrs)?
        } else {
            compile_x86_64_code(&unit.def, &unit.plan, &slot_ptrs)?
        };
        let code_bytes = code.len();
        let mem = alloc_executable(&code)?;
        let entry: JitEntry = unsafe { std::mem::transmute::<*const u8, JitEntry>(mem.ptr) };
        let fn_arc = Arc::new(CompiledJitFn {
            _mem: mem,
            entry,
            param_kinds: unit.param_kinds.clone(),
            return_kind: unit.plan.return_kind,
            code_bytes,
        });
        compiled_units.push((unit.variant_key.clone(), fn_arc));
    }

    // Publish entry pointers into atomic dispatch slots BEFORE exposing in cache.
    for (vkey, fn_arc) in &compiled_units {
        if let Some(atom) = slot_atomics.get(vkey) {
            atom.store(fn_arc.entry as usize, Ordering::Release);
        }
    }

    let mut root_compiled = None;
    {
        let mut guard = jit_cache().lock().map_err(|_| "jit cache poisoned")?;
        for (vkey, fn_arc) in compiled_units {
            stats.compiled_count.fetch_add(1, Ordering::Relaxed);
            stats
                .code_bytes
                .fetch_add(fn_arc.code_bytes, Ordering::Relaxed);
            guard.insert(vkey.clone(), CachedJit::Compiled(fn_arc.clone()));
            if vkey == root_key {
                root_compiled = Some(fn_arc);
            }
        }
    }
    root_compiled.ok_or_else(|| "missing root compiled JIT function".into())
}

#[allow(dead_code)]
fn compile_x86_64_code(
    def: &FunctionDef,
    plan: &VerifiedPlan,
    slot_ptrs: &HashMap<String, usize>,
) -> Result<Vec<u8>, String> {
    let local_count = def.local_count.max(def.params.len());
    let total_slots = local_count + plan.max_stack;
    let slot_bytes = (total_slots * 8 + 15) & !15;
    let frame_bytes = (32 + slot_bytes) as i32;

    struct Asm {
        code: Vec<u8>,
        patches: Vec<(usize, usize)>,
        epilogue_jumps: Vec<usize>,
    }

    impl Asm {
        fn emit_u32(&mut self, v: u32) {
            self.code.extend_from_slice(&v.to_le_bytes());
        }
        fn emit_i32(&mut self, v: i32) {
            self.code.extend_from_slice(&v.to_le_bytes());
        }
        fn emit_u64(&mut self, v: u64) {
            self.code.extend_from_slice(&v.to_le_bytes());
        }
        fn slot_disp(&self, slot: usize) -> i32 {
            (slot * 8) as i32
        }
        fn load_xmm0_slot(&mut self, slot: usize) {
            self.code.extend_from_slice(&[0xF2, 0x0F, 0x10, 0x83]);
            self.emit_i32(self.slot_disp(slot));
        }
        fn load_xmm1_slot(&mut self, slot: usize) {
            self.code.extend_from_slice(&[0xF2, 0x0F, 0x10, 0x8B]);
            self.emit_i32(self.slot_disp(slot));
        }
        #[allow(dead_code)]
        fn load_xmm2_slot(&mut self, slot: usize) {
            self.code.extend_from_slice(&[0xF2, 0x0F, 0x10, 0x93]);
            self.emit_i32(self.slot_disp(slot));
        }
        #[allow(dead_code)]
        fn load_xmm3_slot(&mut self, slot: usize) {
            self.code.extend_from_slice(&[0xF2, 0x0F, 0x10, 0x9B]);
            self.emit_i32(self.slot_disp(slot));
        }
        fn load_rax_slot(&mut self, slot: usize) {
            self.code.extend_from_slice(&[0x48, 0x8B, 0x83]);
            self.emit_i32(self.slot_disp(slot));
        }
        fn store_rax_slot(&mut self, slot: usize) {
            self.code.extend_from_slice(&[0x48, 0x89, 0x83]);
            self.emit_i32(self.slot_disp(slot));
        }
        #[allow(dead_code)]
        fn load_rdx_slot(&mut self, slot: usize) {
            self.code.extend_from_slice(&[0x48, 0x8B, 0x93]);
            self.emit_i32(self.slot_disp(slot));
        }
        #[allow(dead_code)]
        fn load_rsi_slot(&mut self, slot: usize) {
            self.code.extend_from_slice(&[0x48, 0x8B, 0xB3]);
            self.emit_i32(self.slot_disp(slot));
        }
        fn store_xmm0_slot(&mut self, slot: usize) {
            self.code.extend_from_slice(&[0xF2, 0x0F, 0x11, 0x83]);
            self.emit_i32(self.slot_disp(slot));
        }
        fn push_xmm0(&mut self, depth: &mut usize, local_count: usize) {
            let slot = local_count + *depth;
            self.store_xmm0_slot(slot);
            *depth += 1;
        }
        fn push_rax(&mut self, depth: &mut usize, local_count: usize) {
            let slot = local_count + *depth;
            self.store_rax_slot(slot);
            *depth += 1;
        }
        fn pop_rax(&mut self, depth: &mut usize, local_count: usize) -> Result<(), String> {
            if *depth == 0 {
                return Err("jit codegen stack underflow".into());
            }
            *depth -= 1;
            self.load_rax_slot(local_count + *depth);
            Ok(())
        }
        fn pop_xmm0(&mut self, depth: &mut usize, local_count: usize) -> Result<(), String> {
            if *depth == 0 {
                return Err("jit codegen stack underflow".into());
            }
            *depth -= 1;
            self.load_xmm0_slot(local_count + *depth);
            Ok(())
        }
        fn pop_xmm1(&mut self, depth: &mut usize, local_count: usize) -> Result<(), String> {
            if *depth == 0 {
                return Err("jit codegen stack underflow".into());
            }
            *depth -= 1;
            self.load_xmm1_slot(local_count + *depth);
            Ok(())
        }
        fn emit_mov_xmm0_f64(&mut self, v: f64) {
            let bits = v.to_bits();
            self.code.extend_from_slice(&[0x48, 0xB8]);
            self.emit_u64(bits);
            self.code.extend_from_slice(&[0x66, 0x48, 0x0F, 0x6E, 0xC0]);
        }
        fn emit_call_abs(&mut self, addr: usize) {
            self.code.extend_from_slice(&[0x48, 0xB8]);
            self.emit_u64(addr as u64);
            self.code.extend_from_slice(&[0xFF, 0xD0]);
        }
    }

    let mut asm = Asm {
        code: Vec::with_capacity(def.code.len() * 16 + 128),
        patches: Vec::new(),
        epilogue_jumps: Vec::new(),
    };

    // Prologue: save rbp, rbx, r12 (keeps rsp 16-byte aligned), allocate frame, rbx = &slots[0], r12 = ctx
    asm.code.push(0x55); // push rbp
    asm.code.extend_from_slice(&[0x48, 0x89, 0xE5]); // mov rbp, rsp
    asm.code.push(0x53); // push rbx
    asm.code.extend_from_slice(&[0x41, 0x54]); // push r12
    asm.code.extend_from_slice(&[0x48, 0x81, 0xEC]); // sub rsp, imm32
    asm.emit_i32(frame_bytes);
    asm.code.extend_from_slice(&[0x48, 0x8D, 0x5C, 0x24, 0x20]); // lea rbx, [rsp + 32]

    // Preserve ctx in callee-saved r12
    #[cfg(windows)]
    asm.code.extend_from_slice(&[0x4D, 0x89, 0xC4]); // mov r12, r8
    #[cfg(not(windows))]
    asm.code.extend_from_slice(&[0x49, 0x89, 0xD4]); // mov r12, rdx

    // Copy 64-bit parameters from args_ptr into locals[0..arity]
    for i in 0..def.params.len() {
        let disp = (i * 8) as i32;
        #[cfg(windows)]
        asm.code.extend_from_slice(&[0x48, 0x8B, 0x81]); // mov rax, [rcx + disp32]
        #[cfg(not(windows))]
        asm.code.extend_from_slice(&[0x48, 0x8B, 0x87]); // mov rax, [rdi + disp32]
        asm.emit_i32(disp);
        asm.store_rax_slot(i);
    }

    // Zero initialize non-param local slots
    if local_count > def.params.len() {
        asm.code.extend_from_slice(&[0x66, 0x0F, 0x57, 0xC0]); // xorpd xmm0, xmm0
        for i in def.params.len()..local_count {
            asm.store_xmm0_slot(i);
        }
    }

    let mut op_offsets = vec![0usize; def.code.len()];
    for (ip, op) in def.code.iter().enumerate() {
        op_offsets[ip] = asm.code.len();
        let Some(ref st) = plan.stacks_at_ip[ip] else {
            continue;
        };
        let mut depth = st.len();
        match op {
            Op::Const(Val::Num(n)) => {
                asm.emit_mov_xmm0_f64(*n);
                asm.push_xmm0(&mut depth, local_count);
            }
            Op::Const(Val::Bool(b)) => {
                asm.emit_mov_xmm0_f64(if *b { 1.0 } else { 0.0 });
                asm.push_xmm0(&mut depth, local_count);
            }
            Op::Const(Val::Null) => {
                asm.code.extend_from_slice(&[0x66, 0x0F, 0x57, 0xC0]); // xorpd xmm0, xmm0
                asm.push_xmm0(&mut depth, local_count);
            }
            Op::LoadLocal(s) => {
                asm.load_rax_slot(*s);
                asm.push_rax(&mut depth, local_count);
            }
            Op::DefineLocal(s, _, _) | Op::StoreLocal(s) => {
                asm.pop_rax(&mut depth, local_count)?;
                asm.store_rax_slot(*s);
            }
            Op::Pop => {
                if depth == 0 {
                    return Err("pop underflow".into());
                }
            }
            Op::Add | Op::Sub | Op::Mul | Op::Div => {
                asm.pop_xmm1(&mut depth, local_count)?;
                asm.pop_xmm0(&mut depth, local_count)?;
                let opc = match op {
                    Op::Add => 0x58,
                    Op::Sub => 0x5C,
                    Op::Mul => 0x59,
                    _ => 0x5E,
                };
                asm.code.extend_from_slice(&[0xF2, 0x0F, opc, 0xC1]);
                asm.push_xmm0(&mut depth, local_count);
            }
            Op::Mod => {
                asm.pop_xmm1(&mut depth, local_count)?;
                asm.pop_xmm0(&mut depth, local_count)?;
                asm.emit_call_abs(jit_fmod as *const () as usize);
                asm.push_xmm0(&mut depth, local_count);
            }
            Op::Pow => {
                asm.pop_xmm1(&mut depth, local_count)?;
                asm.pop_xmm0(&mut depth, local_count)?;
                asm.emit_call_abs(jit_pow as *const () as usize);
                asm.push_xmm0(&mut depth, local_count);
            }
            Op::Neg => {
                asm.pop_xmm0(&mut depth, local_count)?;
                asm.code.extend_from_slice(&[
                    0x48, 0xB8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80,
                ]); // mov rax, sign_mask
                asm.code.extend_from_slice(&[0x66, 0x48, 0x0F, 0x6E, 0xC8]); // movq xmm1, rax
                asm.code.extend_from_slice(&[0x66, 0x0F, 0x57, 0xC1]); // xorpd xmm0, xmm1
                asm.push_xmm0(&mut depth, local_count);
            }
            Op::Not => {
                asm.pop_xmm0(&mut depth, local_count)?;
                asm.code.extend_from_slice(&[0x31, 0xC0]); // xor eax, eax
                asm.code.extend_from_slice(&[0x66, 0x0F, 0x57, 0xC9]); // xorpd xmm1, xmm1
                asm.code.extend_from_slice(&[0x66, 0x0F, 0x2E, 0xC1]); // ucomisd xmm0, xmm1
                asm.code.extend_from_slice(&[0x0F, 0x94, 0xC0]); // sete al
                asm.code.extend_from_slice(&[0x0F, 0x9A, 0xC2]); // setp dl
                asm.code.extend_from_slice(&[0x08, 0xD0]); // or al, dl
                asm.code.extend_from_slice(&[0xF2, 0x0F, 0x2A, 0xC0]); // cvtsi2sd xmm0, eax
                asm.push_xmm0(&mut depth, local_count);
            }
            Op::Eq | Op::Ne | Op::Lt | Op::Le | Op::Gt | Op::Ge => {
                asm.pop_xmm1(&mut depth, local_count)?;
                asm.pop_xmm0(&mut depth, local_count)?;
                asm.code.extend_from_slice(&[0x31, 0xC0]); // xor eax, eax
                match op {
                    Op::Gt => {
                        asm.code.extend_from_slice(&[0x66, 0x0F, 0x2E, 0xC1]); // ucomisd xmm0, xmm1
                        asm.code.extend_from_slice(&[0x0F, 0x97, 0xC0]); // seta al
                    }
                    Op::Ge => {
                        asm.code.extend_from_slice(&[0x66, 0x0F, 0x2E, 0xC1]); // ucomisd xmm0, xmm1
                        asm.code.extend_from_slice(&[0x0F, 0x93, 0xC0]); // setae al
                    }
                    Op::Lt => {
                        asm.code.extend_from_slice(&[0x66, 0x0F, 0x2E, 0xC8]); // ucomisd xmm1, xmm0
                        asm.code.extend_from_slice(&[0x0F, 0x97, 0xC0]); // seta al
                    }
                    Op::Le => {
                        asm.code.extend_from_slice(&[0x66, 0x0F, 0x2E, 0xC8]); // ucomisd xmm1, xmm0
                        asm.code.extend_from_slice(&[0x0F, 0x93, 0xC0]); // setae al
                    }
                    Op::Eq => {
                        asm.code.extend_from_slice(&[0x66, 0x0F, 0x2E, 0xC1]); // ucomisd xmm0, xmm1
                        asm.code.extend_from_slice(&[0x0F, 0x94, 0xC0]); // sete al
                        asm.code.extend_from_slice(&[0x0F, 0x9B, 0xC2]); // setnp dl
                        asm.code.extend_from_slice(&[0x20, 0xD0]); // and al, dl
                    }
                    Op::Ne => {
                        asm.code.extend_from_slice(&[0x66, 0x0F, 0x2E, 0xC1]); // ucomisd xmm0, xmm1
                        asm.code.extend_from_slice(&[0x0F, 0x95, 0xC0]); // setne al
                        asm.code.extend_from_slice(&[0x0F, 0x9A, 0xC2]); // setp dl
                        asm.code.extend_from_slice(&[0x08, 0xD0]); // or al, dl
                    }
                    _ => unreachable!(),
                }
                asm.code.extend_from_slice(&[0xF2, 0x0F, 0x2A, 0xC0]); // cvtsi2sd xmm0, eax
                asm.push_xmm0(&mut depth, local_count);
            }
            Op::Index => {
                if depth < 2 {
                    return Err("jit index stack underflow".into());
                }
                #[cfg(windows)]
                {
                    depth -= 1;
                    asm.load_xmm2_slot(local_count + depth);
                    depth -= 1;
                    asm.load_rdx_slot(local_count + depth);
                    asm.code.extend_from_slice(&[0x4C, 0x89, 0xE1]); // mov rcx, r12
                }
                #[cfg(not(windows))]
                {
                    asm.pop_xmm0(&mut depth, local_count)?;
                    depth -= 1;
                    asm.load_rsi_slot(local_count + depth);
                    asm.code.extend_from_slice(&[0x4C, 0x89, 0xE7]); // mov rdi, r12
                }
                asm.emit_call_abs(jit_list_get as *const () as usize);
                asm.push_xmm0(&mut depth, local_count);
            }
            Op::SetIndex => {
                if depth < 3 {
                    return Err("jit setindex stack underflow".into());
                }
                #[cfg(windows)]
                {
                    depth -= 1;
                    asm.load_xmm3_slot(local_count + depth);
                    depth -= 1;
                    asm.load_xmm2_slot(local_count + depth);
                    depth -= 1;
                    asm.load_rdx_slot(local_count + depth);
                    asm.code.extend_from_slice(&[0x4C, 0x89, 0xE1]); // mov rcx, r12
                }
                #[cfg(not(windows))]
                {
                    depth -= 1;
                    asm.load_xmm1_slot(local_count + depth);
                    asm.pop_xmm0(&mut depth, local_count)?;
                    depth -= 1;
                    asm.load_rsi_slot(local_count + depth);
                    asm.code.extend_from_slice(&[0x4C, 0x89, 0xE7]); // mov rdi, r12
                }
                asm.emit_call_abs(jit_list_set as *const () as usize);
                asm.push_xmm0(&mut depth, local_count);
            }
            Op::Call(fn_name, argc) => {
                let res = plan
                    .call_resolutions
                    .get(&ip)
                    .ok_or_else(|| format!("missing JIT call resolution for '{fn_name}/{argc}'"))?;
                match res {
                    CallResolution::LenList => {
                        if depth == 0 {
                            return Err("jit len stack underflow".into());
                        }
                        depth -= 1;
                        #[cfg(windows)]
                        {
                            asm.load_rdx_slot(local_count + depth);
                            asm.code.extend_from_slice(&[0x4C, 0x89, 0xE1]); // mov rcx, r12
                        }
                        #[cfg(not(windows))]
                        {
                            asm.load_rsi_slot(local_count + depth);
                            asm.code.extend_from_slice(&[0x4C, 0x89, 0xE7]); // mov rdi, r12
                        }
                        asm.emit_call_abs(jit_list_len as *const () as usize);
                        asm.push_xmm0(&mut depth, local_count);
                    }
                    CallResolution::MathHelper(MathHelper::One(addr)) => {
                        asm.pop_xmm0(&mut depth, local_count)?;
                        asm.emit_call_abs(*addr);
                        asm.push_xmm0(&mut depth, local_count);
                    }
                    CallResolution::MathHelper(MathHelper::Two(addr)) => {
                        asm.pop_xmm1(&mut depth, local_count)?;
                        asm.pop_xmm0(&mut depth, local_count)?;
                        asm.emit_call_abs(*addr);
                        asm.push_xmm0(&mut depth, local_count);
                    }
                    CallResolution::SelfCall
                    | CallResolution::InterFn(_)
                    | CallResolution::AllocList(_) => {
                        let arg_base_depth = depth - *argc;
                        let arg_disp = ((local_count + arg_base_depth) * 8) as i32;
                        depth = arg_base_depth;
                        #[cfg(windows)]
                        {
                            asm.code.extend_from_slice(&[0x48, 0x8D, 0x8B]); // lea rcx, [rbx + disp32]
                            asm.emit_i32(arg_disp);
                            asm.code.push(0xBA); // mov edx, argc
                            asm.emit_u32(*argc as u32);
                            asm.code.extend_from_slice(&[0x4D, 0x89, 0xE0]); // mov r8, r12
                        }
                        #[cfg(not(windows))]
                        {
                            asm.code.extend_from_slice(&[0x48, 0x8D, 0xBB]); // lea rdi, [rbx + disp32]
                            asm.emit_i32(arg_disp);
                            asm.code.push(0xBE); // mov esi, argc
                            asm.emit_u32(*argc as u32);
                            asm.code.extend_from_slice(&[0x4C, 0x89, 0xE2]); // mov rdx, r12
                        }
                        match res {
                            CallResolution::SelfCall => {
                                let call_next = (asm.code.len() + 5) as isize;
                                let rel = -(call_next) as i32;
                                asm.code.push(0xE8);
                                asm.emit_i32(rel);
                            }
                            CallResolution::InterFn(callee_key) => {
                                let slot_addr = *slot_ptrs
                                    .get(callee_key)
                                    .ok_or("missing inter-call atomic slot")?;
                                asm.code.extend_from_slice(&[0x48, 0xB8]); // mov rax, imm64
                                asm.emit_u64(slot_addr as u64);
                                asm.code.extend_from_slice(&[0x48, 0x8B, 0x00]); // mov rax, [rax]
                                asm.code.extend_from_slice(&[0xFF, 0xD0]); // call rax
                            }
                            CallResolution::AllocList(_) => {
                                asm.emit_call_abs(jit_alloc_list as *const () as usize);
                            }
                            _ => unreachable!(),
                        }
                        asm.push_xmm0(&mut depth, local_count);
                    }
                }
            }
            Op::Jump(target) => {
                asm.code.push(0xE9); // jmp rel32
                let patch_at = asm.code.len();
                asm.emit_i32(0);
                asm.patches.push((patch_at, *target));
            }
            Op::JumpIfFalse(target) => {
                asm.pop_xmm0(&mut depth, local_count)?;
                asm.code.extend_from_slice(&[0x66, 0x0F, 0x57, 0xC9]); // xorpd xmm1, xmm1
                asm.code.extend_from_slice(&[0x66, 0x0F, 0x2E, 0xC1]); // ucomisd xmm0, xmm1
                asm.code.extend_from_slice(&[0x0F, 0x84]); // je rel32
                let patch_at = asm.code.len();
                asm.emit_i32(0);
                asm.patches.push((patch_at, *target));
            }
            Op::Return => {
                asm.pop_xmm0(&mut depth, local_count)?;
                asm.code.push(0xE9); // jmp epilogue
                let patch_at = asm.code.len();
                asm.emit_i32(0);
                asm.epilogue_jumps.push(patch_at);
            }
            Op::Halt => {
                asm.code.extend_from_slice(&[0x66, 0x0F, 0x57, 0xC0]); // xorpd xmm0, xmm0
                asm.code.push(0xE9);
                let patch_at = asm.code.len();
                asm.emit_i32(0);
                asm.epilogue_jumps.push(patch_at);
            }
            other => return Err(format!("unsupported JIT op {:?}", other)),
        }
    }

    let epilogue_off = asm.code.len();
    asm.code.extend_from_slice(&[0x48, 0x81, 0xC4]); // add rsp, imm32
    asm.emit_i32(frame_bytes);
    asm.code.extend_from_slice(&[0x41, 0x5C]); // pop r12
    asm.code.push(0x5B); // pop rbx
    asm.code.push(0x5D); // pop rbp
    asm.code.push(0xC3); // ret

    for (patch_at, target_ip) in asm.patches {
        let target_off = *op_offsets
            .get(target_ip)
            .ok_or("jump target offset missing")?;
        let rel = (target_off as isize - (patch_at as isize + 4)) as i32;
        asm.code[patch_at..patch_at + 4].copy_from_slice(&rel.to_le_bytes());
    }
    for patch_at in asm.epilogue_jumps {
        let rel = (epilogue_off as isize - (patch_at as isize + 4)) as i32;
        asm.code[patch_at..patch_at + 4].copy_from_slice(&rel.to_le_bytes());
    }

    Ok(asm.code)
}

/// Programmatic ARM64 (AArch64) machine-code encoder following AAPCS64.
/// Emits native ARMv8-A A64 instructions with X19=slots base, X20=ctx, D0/D1 scratch.
fn compile_aarch64_code(
    def: &FunctionDef,
    plan: &VerifiedPlan,
    slot_ptrs: &HashMap<String, usize>,
) -> Result<Vec<u8>, String> {
    let local_count = def.local_count.max(def.params.len());
    let total_slots = local_count + plan.max_stack;
    let slot_bytes = (total_slots * 8 + 15) & !15;
    if slot_bytes >= 4096 {
        return Err("ARM64 JIT stack frame exceeds single imm12 page".into());
    }
    let frame_imm = slot_bytes as u32;

    enum ArmBranch {
        B(usize, usize),
        Beq(usize, usize),
    }

    struct ArmAsm {
        words: Vec<u32>,
        branches: Vec<ArmBranch>,
        epilogue_jumps: Vec<usize>,
    }

    impl ArmAsm {
        fn emit(&mut self, insn: u32) {
            self.words.push(insn);
        }
        fn ldr_x_slot(&mut self, rt: u32, slot: usize) -> Result<(), String> {
            if slot >= 4095 {
                return Err("slot offset exceeds ARM64 imm12".into());
            }
            // LDR Xt, [X19, #(slot * 8)]
            self.emit(0xF940_0000 | ((slot as u32) << 10) | (19 << 5) | rt);
            Ok(())
        }
        fn str_x_slot(&mut self, rt: u32, slot: usize) -> Result<(), String> {
            if slot >= 4095 {
                return Err("slot offset exceeds ARM64 imm12".into());
            }
            // STR Xt, [X19, #(slot * 8)]
            self.emit(0xF900_0000 | ((slot as u32) << 10) | (19 << 5) | rt);
            Ok(())
        }
        fn ldr_d_slot(&mut self, dt: u32, slot: usize) -> Result<(), String> {
            if slot >= 4095 {
                return Err("slot offset exceeds ARM64 imm12".into());
            }
            // LDR Dt, [X19, #(slot * 8)]
            self.emit(0xFD40_0000 | ((slot as u32) << 10) | (19 << 5) | dt);
            Ok(())
        }
        fn str_d_slot(&mut self, dt: u32, slot: usize) -> Result<(), String> {
            if slot >= 4095 {
                return Err("slot offset exceeds ARM64 imm12".into());
            }
            // STR Dt, [X19, #(slot * 8)]
            self.emit(0xFD00_0000 | ((slot as u32) << 10) | (19 << 5) | dt);
            Ok(())
        }
        fn mov_x_u64(&mut self, rd: u32, val: u64) {
            let p0 = (val & 0xFFFF) as u32;
            let p1 = ((val >> 16) & 0xFFFF) as u32;
            let p2 = ((val >> 32) & 0xFFFF) as u32;
            let p3 = ((val >> 48) & 0xFFFF) as u32;
            // MOVZ Xd, #p0
            self.emit(0xD280_0000 | (p0 << 5) | rd);
            if p1 != 0 {
                self.emit(0xF2A0_0000 | (p1 << 5) | rd);
            }
            if p2 != 0 {
                self.emit(0xF2C0_0000 | (p2 << 5) | rd);
            }
            if p3 != 0 {
                self.emit(0xF2E0_0000 | (p3 << 5) | rd);
            }
        }
        fn push_d0(&mut self, depth: &mut usize, local_count: usize) -> Result<(), String> {
            self.str_d_slot(0, local_count + *depth)?;
            *depth += 1;
            Ok(())
        }
        fn push_x0(&mut self, depth: &mut usize, local_count: usize) -> Result<(), String> {
            self.str_x_slot(0, local_count + *depth)?;
            *depth += 1;
            Ok(())
        }
        fn pop_x(&mut self, rt: u32, depth: &mut usize, local_count: usize) -> Result<(), String> {
            if *depth == 0 {
                return Err("ARM64 jit stack underflow".into());
            }
            *depth -= 1;
            self.ldr_x_slot(rt, local_count + *depth)
        }
        fn pop_d(&mut self, dt: u32, depth: &mut usize, local_count: usize) -> Result<(), String> {
            if *depth == 0 {
                return Err("ARM64 jit stack underflow".into());
            }
            *depth -= 1;
            self.ldr_d_slot(dt, local_count + *depth)
        }
        fn emit_call_abs(&mut self, addr: usize) {
            self.mov_x_u64(16, addr as u64);
            // BLR X16
            self.emit(0xD63F_0200);
        }
    }

    let mut asm = ArmAsm {
        words: Vec::with_capacity(def.code.len() * 6 + 32),
        branches: Vec::new(),
        epilogue_jumps: Vec::new(),
    };

    // Prologue:
    // STP X29, X30, [SP, #-16]!
    asm.emit(0xA9BF_7BFD);
    // MOV X29, SP (ADD X29, SP, #0)
    asm.emit(0x9100_03FD);
    // STP X19, X20, [SP, #-16]!
    asm.emit(0xA9BF_53F3);
    if frame_imm > 0 {
        // SUB SP, SP, #frame_imm
        asm.emit(0xD100_03FF | (frame_imm << 10));
    }
    // MOV X19, SP (ADD X19, SP, #0)
    asm.emit(0x9100_03F3);
    // MOV X20, X2 (ctx pointer)
    asm.emit(0xAA02_03F4);

    // Copy 64-bit parameters from X0 (args_ptr) into locals[0..arity]
    for i in 0..def.params.len() {
        // LDR X9, [X0, #(i*8)]
        asm.emit(0xF940_0009 | ((i as u32) << 10));
        asm.str_x_slot(9, i)?;
    }

    // Zero initialize remaining local slots
    if local_count > def.params.len() {
        for i in def.params.len()..local_count {
            // STR XZR, [X19, #(i*8)]
            asm.str_x_slot(31, i)?;
        }
    }

    let mut op_word_offsets = vec![0usize; def.code.len()];
    for (ip, op) in def.code.iter().enumerate() {
        op_word_offsets[ip] = asm.words.len();
        let Some(ref st) = plan.stacks_at_ip[ip] else {
            continue;
        };
        let mut depth = st.len();
        match op {
            Op::Const(Val::Num(n)) => {
                asm.mov_x_u64(0, n.to_bits());
                // FMOV D0, X0
                asm.emit(0x9E67_0000);
                asm.push_d0(&mut depth, local_count)?;
            }
            Op::Const(Val::Bool(b)) => {
                let bits = if *b { 1.0f64 } else { 0.0f64 }.to_bits();
                asm.mov_x_u64(0, bits);
                asm.emit(0x9E67_0000);
                asm.push_d0(&mut depth, local_count)?;
            }
            Op::Const(Val::Null) => {
                asm.str_x_slot(31, local_count + depth)?;
                depth += 1;
            }
            Op::LoadLocal(s) => {
                asm.ldr_x_slot(0, *s)?;
                asm.push_x0(&mut depth, local_count)?;
            }
            Op::DefineLocal(s, _, _) | Op::StoreLocal(s) => {
                asm.pop_x(0, &mut depth, local_count)?;
                asm.str_x_slot(0, *s)?;
            }
            Op::Pop => {
                if depth == 0 {
                    return Err("pop underflow".into());
                }
            }
            Op::Add | Op::Sub | Op::Mul | Op::Div => {
                asm.pop_d(1, &mut depth, local_count)?;
                asm.pop_d(0, &mut depth, local_count)?;
                let insn = match op {
                    Op::Add => 0x1E61_2800,
                    Op::Sub => 0x1E61_3800,
                    Op::Mul => 0x1E61_0800,
                    _ => 0x1E61_1800,
                };
                asm.emit(insn);
                asm.push_d0(&mut depth, local_count)?;
            }
            Op::Mod => {
                asm.pop_d(1, &mut depth, local_count)?;
                asm.pop_d(0, &mut depth, local_count)?;
                asm.emit_call_abs(jit_fmod as *const () as usize);
                asm.push_d0(&mut depth, local_count)?;
            }
            Op::Pow => {
                asm.pop_d(1, &mut depth, local_count)?;
                asm.pop_d(0, &mut depth, local_count)?;
                asm.emit_call_abs(jit_pow as *const () as usize);
                asm.push_d0(&mut depth, local_count)?;
            }
            Op::Neg => {
                asm.pop_d(0, &mut depth, local_count)?;
                asm.emit(0x1E61_4000); // FNEG D0, D0
                asm.push_d0(&mut depth, local_count)?;
            }
            Op::Not => {
                asm.pop_d(0, &mut depth, local_count)?;
                asm.emit(0x1E60_2008); // FCMP D0, #0.0
                // CSET X0, EQ (cond=0x0, inv=0x1)
                asm.emit(0x9A9F_07E0 | (1 << 12));
                // SCVTF D0, X0
                asm.emit(0x9E62_0000);
                asm.push_d0(&mut depth, local_count)?;
            }
            Op::Eq | Op::Ne | Op::Lt | Op::Le | Op::Gt | Op::Ge => {
                asm.pop_d(1, &mut depth, local_count)?;
                asm.pop_d(0, &mut depth, local_count)?;
                let cond = match op {
                    Op::Gt => {
                        asm.emit(0x1E61_2000); // FCMP D0, D1
                        0xCu32
                    }
                    Op::Ge => {
                        asm.emit(0x1E61_2000);
                        0xAu32
                    }
                    Op::Lt => {
                        asm.emit(0x1E60_2020); // FCMP D1, D0
                        0xCu32
                    }
                    Op::Le => {
                        asm.emit(0x1E60_2020);
                        0xAu32
                    }
                    Op::Eq => {
                        asm.emit(0x1E61_2000);
                        0x0u32
                    }
                    Op::Ne => {
                        asm.emit(0x1E61_2000);
                        0x1u32
                    }
                    _ => unreachable!(),
                };
                // CSET X0, cond => CSINC X0, XZR, XZR, (cond ^ 1)
                asm.emit(0x9A9F_07E0 | ((cond ^ 1) << 12));
                asm.emit(0x9E62_0000); // SCVTF D0, X0
                asm.push_d0(&mut depth, local_count)?;
            }
            Op::Index => {
                asm.pop_d(0, &mut depth, local_count)?;
                asm.pop_x(1, &mut depth, local_count)?;
                // MOV X0, X20 (ORR X0, XZR, X20)
                asm.emit(0xAA14_03E0);
                asm.emit_call_abs(jit_list_get as *const () as usize);
                asm.push_d0(&mut depth, local_count)?;
            }
            Op::SetIndex => {
                asm.pop_d(1, &mut depth, local_count)?;
                asm.pop_d(0, &mut depth, local_count)?;
                asm.pop_x(1, &mut depth, local_count)?;
                // MOV X0, X20 (ORR X0, XZR, X20)
                asm.emit(0xAA14_03E0);
                asm.emit_call_abs(jit_list_set as *const () as usize);
                asm.push_d0(&mut depth, local_count)?;
            }
            Op::Call(fn_name, argc) => {
                let res = plan
                    .call_resolutions
                    .get(&ip)
                    .ok_or_else(|| format!("missing JIT call resolution for '{fn_name}/{argc}'"))?;
                match res {
                    CallResolution::LenList => {
                        asm.pop_x(1, &mut depth, local_count)?;
                        asm.emit(0xAA14_03E0); // MOV X0, X20
                        asm.emit_call_abs(jit_list_len as *const () as usize);
                        asm.push_d0(&mut depth, local_count)?;
                    }
                    CallResolution::MathHelper(MathHelper::One(addr)) => {
                        asm.pop_d(0, &mut depth, local_count)?;
                        asm.emit_call_abs(*addr);
                        asm.push_d0(&mut depth, local_count)?;
                    }
                    CallResolution::MathHelper(MathHelper::Two(addr)) => {
                        asm.pop_d(1, &mut depth, local_count)?;
                        asm.pop_d(0, &mut depth, local_count)?;
                        asm.emit_call_abs(*addr);
                        asm.push_d0(&mut depth, local_count)?;
                    }
                    CallResolution::SelfCall
                    | CallResolution::InterFn(_)
                    | CallResolution::AllocList(_) => {
                        let arg_base_depth = depth - *argc;
                        let arg_disp_bytes = ((local_count + arg_base_depth) * 8) as u32;
                        if arg_disp_bytes >= 4096 {
                            return Err("ARM64 call arg offset exceeds imm12".into());
                        }
                        depth = arg_base_depth;
                        // ADD X0, X19, #arg_disp_bytes
                        asm.emit(0x9100_0260 | (arg_disp_bytes << 10));
                        // MOVZ X1, #argc
                        asm.emit(0xD280_0001 | ((*argc as u32) << 5));
                        // MOV X2, X20
                        asm.emit(0xAA14_03E2);
                        match res {
                            CallResolution::SelfCall => {
                                let cur_word = asm.words.len() as isize;
                                let delta = (-cur_word) as u32;
                                asm.emit(0x9400_0000 | (delta & 0x03FF_FFFF)); // BL 0
                            }
                            CallResolution::InterFn(callee_key) => {
                                let slot_addr = *slot_ptrs
                                    .get(callee_key)
                                    .ok_or("missing inter-call atomic slot")?;
                                asm.mov_x_u64(16, slot_addr as u64);
                                // LDR X16, [X16, #0]
                                asm.emit(0xF940_0210);
                                // BLR X16
                                asm.emit(0xD63F_0200);
                            }
                            CallResolution::AllocList(_) => {
                                asm.emit_call_abs(jit_alloc_list as *const () as usize);
                            }
                            _ => unreachable!(),
                        }
                        asm.push_d0(&mut depth, local_count)?;
                    }
                }
            }
            Op::Jump(target) => {
                let idx = asm.words.len();
                asm.emit(0x1400_0000);
                asm.branches.push(ArmBranch::B(idx, *target));
            }
            Op::JumpIfFalse(target) => {
                asm.pop_d(0, &mut depth, local_count)?;
                asm.emit(0x1E60_2008); // FCMP D0, #0.0
                let idx = asm.words.len();
                asm.emit(0x5400_0000); // B.EQ placeholder
                asm.branches.push(ArmBranch::Beq(idx, *target));
            }
            Op::Return => {
                asm.pop_d(0, &mut depth, local_count)?;
                let idx = asm.words.len();
                asm.emit(0x1400_0000);
                asm.epilogue_jumps.push(idx);
            }
            Op::Halt => {
                // FMOV D0, XZR
                asm.emit(0x9E67_03E0);
                let idx = asm.words.len();
                asm.emit(0x1400_0000);
                asm.epilogue_jumps.push(idx);
            }
            other => return Err(format!("unsupported ARM64 JIT op {:?}", other)),
        }
    }

    let epilogue_word = asm.words.len();
    if frame_imm > 0 {
        // ADD SP, SP, #frame_imm
        asm.emit(0x9100_03FF | (frame_imm << 10));
    }
    // LDP X19, X20, [SP], #16
    asm.emit(0xA8C1_53F3);
    // LDP X29, X30, [SP], #16
    asm.emit(0xA8C1_7BFD);
    // RET
    asm.emit(0xD65F_03C0);

    for b in asm.branches {
        match b {
            ArmBranch::B(idx, target_ip) => {
                let target_w = *op_word_offsets
                    .get(target_ip)
                    .ok_or("ARM64 target offset missing")?;
                let delta = (target_w as isize - idx as isize) as u32;
                asm.words[idx] = 0x1400_0000 | (delta & 0x03FF_FFFF);
            }
            ArmBranch::Beq(idx, target_ip) => {
                let target_w = *op_word_offsets
                    .get(target_ip)
                    .ok_or("ARM64 target offset missing")?;
                let delta = (target_w as isize - idx as isize) as u32;
                asm.words[idx] = 0x5400_0000 | ((delta & 0x7_FFFF) << 5);
            }
        }
    }
    for idx in asm.epilogue_jumps {
        let delta = (epilogue_word as isize - idx as isize) as u32;
        asm.words[idx] = 0x1400_0000 | (delta & 0x03FF_FFFF);
    }

    let mut bytes = Vec::with_capacity(asm.words.len() * 4);
    for w in asm.words {
        bytes.extend_from_slice(&w.to_le_bytes());
    }
    Ok(bytes)
}

#[cfg(test)]
pub(crate) fn debug_emit_aarch64(
    name: &str,
    def: &FunctionDef,
    callee_lookup: &dyn Fn(&str) -> Option<FunctionDef>,
) -> Result<Vec<u32>, String> {
    let kinds = infer_param_kinds(def)?;
    let mut planned = HashMap::new();
    let mut active = HashSet::new();
    let _ = verify_and_collect_plans(
        name,
        def,
        &kinds,
        callee_lookup,
        &mut planned,
        &mut active,
        false,
    )?;
    let root_key = jit_variant_key(name, def, &kinds);
    let mut slot_ptrs = HashMap::new();
    for unit in planned.values() {
        for res in unit.plan.call_resolutions.values() {
            if let CallResolution::InterFn(k) = res {
                slot_ptrs.entry(k.clone()).or_insert(0x1000_0000usize);
            }
        }
    }
    let root = planned.get(&root_key).ok_or("missing planned root")?;
    let bytes = compile_aarch64_code(&root.def, &root.plan, &slot_ptrs)?;
    Ok(bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect())
}

#[cfg(all(any(target_arch = "x86_64", target_arch = "aarch64"), windows))]
pub(crate) fn alloc_executable(bytes: &[u8]) -> Result<ExecMem, String> {
    const MEM_COMMIT: u32 = 0x1000;
    const MEM_RESERVE: u32 = 0x2000;
    const PAGE_READWRITE: u32 = 0x04;
    const PAGE_EXECUTE_READ: u32 = 0x20;
    unsafe extern "system" {
        fn VirtualAlloc(
            lpAddress: *mut core::ffi::c_void,
            dwSize: usize,
            flAllocationType: u32,
            flProtect: u32,
        ) -> *mut core::ffi::c_void;
        fn VirtualProtect(
            lpAddress: *mut core::ffi::c_void,
            dwSize: usize,
            flNewProtect: u32,
            lpflOldProtect: *mut u32,
        ) -> i32;
        fn GetCurrentProcess() -> *mut core::ffi::c_void;
        fn FlushInstructionCache(
            hProcess: *mut core::ffi::c_void,
            lpBaseAddress: *const core::ffi::c_void,
            dwSize: usize,
        ) -> i32;
    }
    let size = bytes.len().max(64);
    let ptr = unsafe {
        VirtualAlloc(
            core::ptr::null_mut(),
            size,
            MEM_COMMIT | MEM_RESERVE,
            PAGE_READWRITE,
        ) as *mut u8
    };
    if ptr.is_null() {
        return Err("VirtualAlloc failed allocating JIT code memory".into());
    }
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
    }
    let mut old_protect: u32 = 0;
    let ok = unsafe {
        VirtualProtect(
            ptr as *mut core::ffi::c_void,
            size,
            PAGE_EXECUTE_READ,
            &mut old_protect,
        )
    };
    if ok == 0 {
        return Err("VirtualProtect(PAGE_EXECUTE_READ) failed".into());
    }
    unsafe {
        let _ = FlushInstructionCache(GetCurrentProcess(), ptr as *const core::ffi::c_void, size);
    }
    Ok(ExecMem { ptr, size })
}

#[cfg(all(any(target_arch = "x86_64", target_arch = "aarch64"), unix))]
pub(crate) fn alloc_executable(bytes: &[u8]) -> Result<ExecMem, String> {
    const PROT_READ: i32 = 0x1;
    const PROT_WRITE: i32 = 0x2;
    const PROT_EXEC: i32 = 0x4;
    const MAP_PRIVATE: i32 = 0x02;
    #[cfg(target_os = "macos")]
    const MAP_ANON: i32 = 0x1000;
    #[cfg(not(target_os = "macos"))]
    const MAP_ANON: i32 = 0x20;
    unsafe extern "C" {
        fn mmap(
            addr: *mut core::ffi::c_void,
            len: usize,
            prot: i32,
            flags: i32,
            fd: i32,
            offset: isize,
        ) -> *mut core::ffi::c_void;
        fn mprotect(addr: *mut core::ffi::c_void, len: usize, prot: i32) -> i32;
        fn munmap(addr: *mut core::ffi::c_void, len: usize) -> i32;
    }
    let size = bytes.len().max(4096);
    let ptr = unsafe {
        mmap(
            core::ptr::null_mut(),
            size,
            PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_ANON,
            -1,
            0,
        )
    };
    if ptr.is_null() || ptr as isize == -1 {
        return Err("mmap failed allocating JIT code memory".into());
    }
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, bytes.len());
    }
    if unsafe { mprotect(ptr, size, PROT_READ | PROT_EXEC) } != 0 {
        unsafe {
            let _ = munmap(ptr, size);
        }
        return Err("mprotect(PROT_EXEC) failed".into());
    }
    #[cfg(target_arch = "aarch64")]
    {
        unsafe extern "C" {
            fn __clear_cache(start: *mut core::ffi::c_void, end: *mut core::ffi::c_void);
        }
        unsafe {
            let start = ptr;
            let end = (ptr as *mut u8).add(bytes.len()) as *mut core::ffi::c_void;
            __clear_cache(start, end);
        }
    }
    Ok(ExecMem {
        ptr: ptr as *mut u8,
        size,
    })
}

#[cfg(not(any(
    all(target_arch = "x86_64", any(windows, unix)),
    all(target_arch = "aarch64", any(windows, unix))
)))]
pub(crate) fn alloc_executable(_bytes: &[u8]) -> Result<ExecMem, String> {
    Err("native JIT requires x86_64 or aarch64 host".into())
}
