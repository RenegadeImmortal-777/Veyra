// Veyra native JIT compiler: translates hot numeric bytecode functions into
// in-memory executable x86_64 machine code using SSE2 scalar arithmetic and
// W^X memory protection, with clean interpreted fallback on unsupported ops or
// non-x86_64 architectures.

#![allow(clippy::too_many_arguments)]

use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
};

use crate::bytecode::{FunctionDef, Op};
use crate::value::Val;

pub(crate) type JitEntry = unsafe extern "C" fn(args_ptr: *const f64, argc: usize) -> f64;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum JitReturnKind {
    Num,
    Bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum AbstractType {
    Num,
    Bool,
}

struct ExecMem {
    ptr: *mut u8,
    #[allow(dead_code)]
    size: usize,
}

// Safety: Executable code pages are read-execute (RX) immutable machine code.
unsafe impl Send for ExecMem {}
unsafe impl Sync for ExecMem {}

impl Drop for ExecMem {
    fn drop(&mut self) {
        #[cfg(all(target_arch = "x86_64", windows))]
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
        #[cfg(all(target_arch = "x86_64", unix))]
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
    arity: usize,
    return_kind: JitReturnKind,
    code_bytes: usize,
}

// Safety: `entry` points into immutable RX memory owned by `_mem` and only reads args slice.
unsafe impl Send for CompiledJitFn {}
unsafe impl Sync for CompiledJitFn {}

impl CompiledJitFn {
    pub(crate) fn invoke(&self, args: &[Val]) -> Option<Val> {
        if args.len() != self.arity {
            return None;
        }
        let mut raw_args = [0.0f64; 16];
        let arg_buf: &[f64];
        let heap_args: Vec<f64>;
        if args.len() <= raw_args.len() {
            for (i, v) in args.iter().enumerate() {
                match v {
                    Val::Num(n) => raw_args[i] = *n,
                    _ => return None,
                }
            }
            arg_buf = &raw_args[..args.len()];
        } else {
            let mut vec = Vec::with_capacity(args.len());
            for v in args {
                match v {
                    Val::Num(n) => vec.push(*n),
                    _ => return None,
                }
            }
            heap_args = vec;
            arg_buf = &heap_args;
        }
        let out = unsafe { (self.entry)(arg_buf.as_ptr(), arg_buf.len()) };
        Some(match self.return_kind {
            JitReturnKind::Num => Val::Num(out),
            JitReturnKind::Bool => Val::Bool(out != 0.0 && !out.is_nan()),
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
static JIT_STATS: OnceLock<JitStats> = OnceLock::new();

fn jit_cache() -> &'static Mutex<HashMap<String, CachedJit>> {
    JIT_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
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
    cfg!(all(target_arch = "x86_64", any(windows, unix)))
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

pub(crate) fn jit_cache_key(name: &str, def: &FunctionDef) -> String {
    format!("{name}@{:x}", Arc::as_ptr(&def.code) as usize)
}

pub(crate) fn jit_try_compile(name: &str, def: &FunctionDef) -> Result<Arc<CompiledJitFn>, String> {
    let key = jit_cache_key(name, def);
    {
        let guard = jit_cache().lock().map_err(|_| "jit cache poisoned")?;
        if let Some(entry) = guard.get(&key) {
            return match entry {
                CachedJit::Compiled(f) => Ok(f.clone()),
                CachedJit::Unsupported(reason) => Err(reason.clone()),
            };
        }
    }
    let res = compile_function_impl(name, def);
    let stats = jit_stats_store();
    let mut guard = jit_cache().lock().map_err(|_| "jit cache poisoned")?;
    match &res {
        Ok(compiled) => {
            stats.compiled_count.fetch_add(1, Ordering::Relaxed);
            stats
                .code_bytes
                .fetch_add(compiled.code_bytes, Ordering::Relaxed);
            guard.insert(key, CachedJit::Compiled(compiled.clone()));
        }
        Err(reason) => {
            stats.rejected_count.fetch_add(1, Ordering::Relaxed);
            guard.insert(key, CachedJit::Unsupported(reason.clone()));
        }
    }
    res
}

pub(crate) fn jit_try_run(
    name: &str,
    def: &FunctionDef,
    args: &[Val],
    call_count: u32,
) -> Option<Val> {
    if !jit_is_enabled() {
        return None;
    }
    if def.is_async || def.is_generator || !def.captures.is_empty() || !def.upvalues.is_empty() {
        return None;
    }
    if !args.iter().all(|v| matches!(v, Val::Num(_))) {
        return None;
    }
    if def.params.iter().zip(args).any(|((_, ty), v)| {
        ty.as_deref() == Some("Int") && !matches!(v, Val::Num(n) if n.fract() == 0.0)
    }) {
        return None;
    }
    let key = jit_cache_key(name, def);
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
        match jit_try_compile(name, def) {
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
    let backend = if jit_available() {
        "x86_64-sse2"
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

#[allow(dead_code)]
fn type_is_numeric_compat(ty: Option<&String>) -> bool {
    matches!(
        ty.map(String::as_str),
        None | Some("Any") | Some("Number") | Some("Int")
    )
}

struct VerifiedPlan {
    stacks_at_ip: Vec<Option<Vec<AbstractType>>>,
    max_stack: usize,
    return_kind: JitReturnKind,
}

fn verify_numeric_cfg(name: &str, def: &FunctionDef) -> Result<VerifiedPlan, String> {
    if def.is_async || def.is_generator {
        return Err("async/generator functions use VM runtime".into());
    }
    if !def.captures.is_empty() || !def.upvalues.is_empty() {
        return Err("closure upvalues require VM frame".into());
    }
    for (_, pty) in def.params.iter() {
        if !type_is_numeric_compat(pty.as_ref()) {
            return Err("non-numeric parameter type annotation".into());
        }
    }
    match def.return_type.as_deref() {
        None | Some("Any") | Some("Number") | Some("Bool") => {}
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

    let n = code.len();
    let mut stacks_at_ip: Vec<Option<Vec<AbstractType>>> = vec![None; n];
    let mut local_types: Vec<Option<AbstractType>> =
        vec![None; def.local_count.max(def.params.len())];
    for slot in &mut local_types[..def.params.len()] {
        *slot = Some(AbstractType::Num);
    }
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
                    if !type_is_numeric_compat(ty.as_ref()) {
                        return Err("non-numeric local type annotation".into());
                    }
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
                    if a != b {
                        return Err("mixed comparison types".into());
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
                    stack.pop().ok_or("stack underflow")?;
                    stack.push(AbstractType::Bool);
                }
                Op::Call(fn_name, argc) => {
                    if stack.len() < *argc {
                        return Err("call stack underflow".into());
                    }
                    for _ in 0..*argc {
                        if stack.pop() != Some(AbstractType::Num) {
                            return Err("JIT call arguments must be numbers".into());
                        }
                    }
                    let is_supported = if fn_name == name && *argc == def.params.len() {
                        matches!(
                            def.return_type.as_deref(),
                            None | Some("Any") | Some("Number") | Some("Int")
                        )
                    } else {
                        math_helper_kind(fn_name, *argc).is_some()
                    };
                    if !is_supported {
                        return Err(format!("unsupported JIT call '{fn_name}/{argc}'"));
                    }
                    stack.push(AbstractType::Num);
                }
                Op::Jump(target) => {
                    push_edge(*target, &stack, &mut stacks_at_ip, &mut worklist)?;
                    break;
                }
                Op::JumpIfFalse(target) => {
                    stack.pop().ok_or("stack underflow")?;
                    push_edge(*target, &stack, &mut stacks_at_ip, &mut worklist)?;
                    push_edge(cur + 1, &stack, &mut stacks_at_ip, &mut worklist)?;
                    break;
                }
                Op::Return => {
                    let r = stack.pop().ok_or("return stack underflow")?;
                    let rk = match r {
                        AbstractType::Num => JitReturnKind::Num,
                        AbstractType::Bool => JitReturnKind::Bool,
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
    Ok(VerifiedPlan {
        stacks_at_ip,
        max_stack: max_stack + 4,
        return_kind,
    })
}

#[derive(Clone, Copy)]
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
unsafe extern "C" fn jit_min(a: f64, b: f64) -> f64 {
    a.min(b)
}
unsafe extern "C" fn jit_max(a: f64, b: f64) -> f64 {
    a.max(b)
}

fn math_helper_kind(name: &str, argc: usize) -> Option<MathHelper> {
    match (name, argc) {
        ("abs", 1) => Some(MathHelper::One(jit_abs as *const () as usize)),
        ("sqrt", 1) => Some(MathHelper::One(jit_sqrt as *const () as usize)),
        ("sin", 1) => Some(MathHelper::One(jit_sin as *const () as usize)),
        ("cos", 1) => Some(MathHelper::One(jit_cos as *const () as usize)),
        ("tan", 1) => Some(MathHelper::One(jit_tan as *const () as usize)),
        ("exp", 1) => Some(MathHelper::One(jit_exp as *const () as usize)),
        ("log", 1) => Some(MathHelper::One(jit_log as *const () as usize)),
        ("floor", 1) => Some(MathHelper::One(jit_floor as *const () as usize)),
        ("ceil", 1) => Some(MathHelper::One(jit_ceil as *const () as usize)),
        ("round", 1) => Some(MathHelper::One(jit_round as *const () as usize)),
        ("pow", 2) => Some(MathHelper::Two(jit_pow as *const () as usize)),
        ("min", 2) => Some(MathHelper::Two(jit_min as *const () as usize)),
        ("max", 2) => Some(MathHelper::Two(jit_max as *const () as usize)),
        _ => None,
    }
}

#[cfg(all(target_arch = "x86_64", any(windows, unix)))]
fn compile_function_impl(name: &str, def: &FunctionDef) -> Result<Arc<CompiledJitFn>, String> {
    let plan = verify_numeric_cfg(name, def)?;
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
        fn store_xmm0_slot(&mut self, slot: usize) {
            self.code.extend_from_slice(&[0xF2, 0x0F, 0x11, 0x83]);
            self.emit_i32(self.slot_disp(slot));
        }
        fn push_xmm0(&mut self, depth: &mut usize, local_count: usize) {
            let slot = local_count + *depth;
            self.store_xmm0_slot(slot);
            *depth += 1;
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

    // Prologue: save rbp, rbx, r12 (keeps rsp 16-byte aligned), allocate frame, rbx = &slots[0]
    asm.code.push(0x55); // push rbp
    asm.code.extend_from_slice(&[0x48, 0x89, 0xE5]); // mov rbp, rsp
    asm.code.push(0x53); // push rbx
    asm.code.extend_from_slice(&[0x41, 0x54]); // push r12
    asm.code.extend_from_slice(&[0x48, 0x81, 0xEC]); // sub rsp, imm32
    asm.emit_i32(frame_bytes);
    asm.code.extend_from_slice(&[0x48, 0x8D, 0x5C, 0x24, 0x20]); // lea rbx, [rsp + 32]

    // Copy parameters from args_ptr (rcx on Win64, rdi on SysV) into locals[0..arity]
    for i in 0..def.params.len() {
        let disp = (i * 8) as i32;
        #[cfg(windows)]
        asm.code.extend_from_slice(&[0xF2, 0x0F, 0x10, 0x81]); // movsd xmm0, [rcx + disp32]
        #[cfg(not(windows))]
        asm.code.extend_from_slice(&[0xF2, 0x0F, 0x10, 0x87]); // movsd xmm0, [rdi + disp32]
        asm.emit_i32(disp);
        asm.store_xmm0_slot(i);
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
            // Unreachable bytecode: still keep offset valid without breaking lowering.
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
                asm.load_xmm0_slot(*s);
                asm.push_xmm0(&mut depth, local_count);
            }
            Op::DefineLocal(s, _, _) | Op::StoreLocal(s) => {
                asm.pop_xmm0(&mut depth, local_count)?;
                asm.store_xmm0_slot(*s);
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
            Op::Call(fn_name, argc) => {
                if fn_name == name && *argc == def.params.len() {
                    let arg_base_depth = depth - *argc;
                    let arg_disp = ((local_count + arg_base_depth) * 8) as i32;
                    depth = arg_base_depth;
                    #[cfg(windows)]
                    {
                        asm.code.extend_from_slice(&[0x48, 0x8D, 0x8B]); // lea rcx, [rbx + disp32]
                        asm.emit_i32(arg_disp);
                        asm.code.push(0xBA); // mov edx, argc
                        asm.emit_u32(*argc as u32);
                    }
                    #[cfg(not(windows))]
                    {
                        asm.code.extend_from_slice(&[0x48, 0x8D, 0xBB]); // lea rdi, [rbx + disp32]
                        asm.emit_i32(arg_disp);
                        asm.code.push(0xBE); // mov esi, argc
                        asm.emit_u32(*argc as u32);
                    }
                    // Self-call directly to function prologue at offset 0.
                    let call_next = (asm.code.len() + 5) as isize;
                    let rel = -(call_next) as i32;
                    asm.code.push(0xE8);
                    asm.emit_i32(rel);
                    asm.push_xmm0(&mut depth, local_count);
                } else {
                    match math_helper_kind(fn_name, *argc) {
                        Some(MathHelper::One(addr)) => {
                            asm.pop_xmm0(&mut depth, local_count)?;
                            asm.emit_call_abs(addr);
                            asm.push_xmm0(&mut depth, local_count);
                        }
                        Some(MathHelper::Two(addr)) => {
                            asm.pop_xmm1(&mut depth, local_count)?;
                            asm.pop_xmm0(&mut depth, local_count)?;
                            asm.emit_call_abs(addr);
                            asm.push_xmm0(&mut depth, local_count);
                        }
                        None => return Err(format!("unsupported JIT call '{fn_name}/{argc}'")),
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

    let code_bytes = asm.code.len();
    let mem = alloc_executable(&asm.code)?;
    let entry: JitEntry = unsafe { std::mem::transmute::<*const u8, JitEntry>(mem.ptr) };
    Ok(Arc::new(CompiledJitFn {
        _mem: mem,
        entry,
        arity: def.params.len(),
        return_kind: plan.return_kind,
        code_bytes,
    }))
}

#[cfg(not(all(target_arch = "x86_64", any(windows, unix))))]
fn compile_function_impl(_name: &str, _def: &FunctionDef) -> Result<Arc<CompiledJitFn>, String> {
    Err("native JIT requires x86_64 host".into())
}

#[cfg(all(target_arch = "x86_64", windows))]
fn alloc_executable(bytes: &[u8]) -> Result<ExecMem, String> {
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

#[cfg(all(target_arch = "x86_64", unix))]
fn alloc_executable(bytes: &[u8]) -> Result<ExecMem, String> {
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
    Ok(ExecMem {
        ptr: ptr as *mut u8,
        size,
    })
}
