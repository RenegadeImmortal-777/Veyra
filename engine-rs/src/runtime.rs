// Shared runtime state: object, task, generator, channel and function-object
// registries, plus the namespace helpers used when a module is imported.
//
// Extracted from `main.rs` without behavior change; see docs/ENGINE-SPLIT.md.

use std::collections::{HashMap, HashSet};
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicU64, Ordering},
    mpsc::{Receiver, Sender},
};
use std::thread;

use crate::bytecode::{FunctionDef, Op};
use crate::value::Val;

pub(crate) static OBJECT_IDS: AtomicU64 = AtomicU64::new(1);
pub(crate) static TASK_IDS: AtomicU64 = AtomicU64::new(1);
pub(crate) static GENERATOR_IDS: AtomicU64 = AtomicU64::new(1);
pub(crate) static CHANNEL_IDS: AtomicU64 = AtomicU64::new(1);
pub(crate) static FUNCTION_IDS: AtomicU64 = AtomicU64::new(1);
pub(crate) static FUNCTION_OBJECTS: OnceLock<Mutex<HashMap<String, FunctionDef>>> = OnceLock::new();

pub(crate) static TASKS: OnceLock<Mutex<HashMap<u64, thread::JoinHandle<Result<Val, String>>>>> =
    OnceLock::new();
pub(crate) static CHANNELS: OnceLock<
    Mutex<HashMap<u64, (Sender<Val>, Arc<Mutex<Receiver<Val>>>)>>,
> = OnceLock::new();

pub(crate) fn tasks() -> &'static Mutex<HashMap<u64, thread::JoinHandle<Result<Val, String>>>> {
    TASKS.get_or_init(|| Mutex::new(HashMap::new()))
}
pub(crate) fn channels() -> &'static Mutex<HashMap<u64, (Sender<Val>, Arc<Mutex<Receiver<Val>>>)>> {
    CHANNELS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn function_objects() -> &'static Mutex<HashMap<String, FunctionDef>> {
    FUNCTION_OBJECTS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn namespace_module_ops(code: &mut [Op], local_names: &HashSet<String>, prefix: &str) {
    for op in code {
        match op {
            Op::Call(name, _) | Op::FunctionRef(name) | Op::CallExt(0, name, _) => {
                if local_names.contains(name) {
                    *name = format!("{prefix}{name}");
                }
            }
            Op::Def(_, _, _, _, nested) => namespace_module_ops(nested, local_names, prefix),
            Op::DefClass(_, _, methods) => {
                for (_, _, _, _, nested) in methods {
                    namespace_module_ops(nested, local_names, prefix);
                }
            }
            Op::BuildClass(_, _, body) => {
                namespace_module_ops(body, local_names, prefix);
            }
            _ => {}
        }
    }
}

pub(crate) fn export_function_object(def: FunctionDef) -> Result<String, String> {
    let id = FUNCTION_IDS.fetch_add(1, Ordering::Relaxed);
    let key = format!("@fn:{id}");
    function_objects()
        .lock()
        .map_err(|_| "function object registry poisoned")?
        .insert(key.clone(), def);
    Ok(key)
}

pub(crate) fn register_named_function_object(name: &str, def: FunctionDef) -> Result<(), String> {
    function_objects()
        .lock()
        .map_err(|_| "function object registry poisoned")?
        .insert(name.to_string(), def);
    Ok(())
}

pub(crate) fn resolve_function_object(name: &str) -> Result<Option<FunctionDef>, String> {
    Ok(function_objects()
        .lock()
        .map_err(|_| "function object registry poisoned")?
        .get(name)
        .cloned())
}
