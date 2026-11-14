// Shared runtime state: object, task, generator, channel and function-object
// registries, plus the namespace helpers used when a module is imported.
//
// Extracted from `main.rs` without behavior change; see docs/ENGINE-SPLIT.md.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::env;
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

thread_local! {
    static ACTIVE_CAPS: RefCell<Option<HashSet<String>>> = const { RefCell::new(None) };
}

fn parse_cap_set<'a>(caps: impl IntoIterator<Item = &'a str>) -> HashSet<String> {
    caps.into_iter()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn env_cap_override() -> Option<HashSet<String>> {
    let raw = env::var("KOVREK_CAPS").ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "*" || trimmed.eq_ignore_ascii_case("all") {
        return None;
    }
    Some(parse_cap_set(trimmed.split(',')))
}

pub(crate) fn require_cap(cap: &str) -> Result<(), String> {
    ACTIVE_CAPS.with(|cell| {
        let borrow = cell.borrow();
        let active = borrow.as_ref();
        if let Some(set) = active {
            if set.contains("*") || set.contains("all") || set.contains(cap) {
                return Ok(());
            }
            if let Some((cat, _)) = cap.split_once('.')
                && set.contains(cat)
            {
                return Ok(());
            }
            return Err(format!(
                "CapabilityError: missing required capability '{cap}'"
            ));
        }
        if let Some(env_set) = env_cap_override() {
            if env_set.contains(cap)
                || cap
                    .split_once('.')
                    .is_some_and(|(cat, _)| env_set.contains(cat))
            {
                return Ok(());
            }
            return Err(format!(
                "CapabilityError: missing required capability '{cap}'"
            ));
        }
        Ok(())
    })
}

pub(crate) fn current_caps() -> Option<Vec<String>> {
    ACTIVE_CAPS.with(|cell| {
        let borrow = cell.borrow();
        if let Some(set) = borrow.as_ref() {
            let mut out: Vec<String> = set.iter().cloned().collect();
            out.sort();
            return Some(out);
        }
        env_cap_override().map(|set| {
            let sorted: BTreeSet<String> = set.into_iter().collect();
            sorted.into_iter().collect()
        })
    })
}

struct CapGuard {
    prev: Option<HashSet<String>>,
}

impl Drop for CapGuard {
    fn drop(&mut self) {
        let prev = self.prev.take();
        ACTIVE_CAPS.with(|cell| {
            *cell.borrow_mut() = prev;
        });
    }
}

pub(crate) fn run_with_caps<T>(
    requested: Vec<String>,
    f: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let prev = ACTIVE_CAPS.with(|cell| cell.borrow().clone());
    let base = prev.clone().or_else(env_cap_override);
    let req_set = parse_cap_set(requested.iter().map(String::as_str));
    let attenuated = match base {
        None => {
            if req_set.contains("*") || req_set.contains("all") {
                None
            } else {
                Some(req_set)
            }
        }
        Some(outer) => {
            if req_set.contains("*") || req_set.contains("all") {
                Some(outer)
            } else {
                // Monotonic attenuation: inner scope can NEVER escalate beyond outer capabilities.
                Some(outer.into_iter().filter(|c| req_set.contains(c)).collect())
            }
        }
    };
    ACTIVE_CAPS.with(|cell| {
        *cell.borrow_mut() = attenuated;
    });
    let _guard = CapGuard { prev };
    f()
}

thread_local! {
    static PRINT_CAPTURE: RefCell<Option<String>> = const { RefCell::new(None) };
}

pub(crate) fn try_capture_print(text: &str) -> bool {
    PRINT_CAPTURE.with(|cell| {
        if let Some(buf) = cell.borrow_mut().as_mut() {
            buf.push_str(text);
            return true;
        }
        false
    })
}

pub(crate) fn with_print_capture<T>(f: impl FnOnce() -> T) -> (T, String) {
    struct PrintCaptureGuard(Option<String>);
    impl Drop for PrintCaptureGuard {
        fn drop(&mut self) {
            let prev = self.0.take();
            PRINT_CAPTURE.with(|cell| {
                *cell.borrow_mut() = prev;
            });
        }
    }
    let prev = PRINT_CAPTURE.with(|cell| cell.borrow_mut().replace(String::new()));
    let _guard = PrintCaptureGuard(prev);
    let out = f();
    let captured = PRINT_CAPTURE
        .with(|cell| cell.borrow_mut().take())
        .unwrap_or_default();
    (out, captured)
}

/// Displays an optional prompt without a trailing newline and reads one line
/// from stdin. Returns `Ok(None)` cleanly at EOF.
pub(crate) fn read_console_input(prompt: Option<&str>) -> Result<Option<String>, String> {
    use std::io::{self, BufRead, Write};
    if let Some(p) = prompt
        && !p.is_empty()
        && !try_capture_print(p)
    {
        print!("{p}");
        io::stdout().flush().map_err(|e| e.to_string())?;
    }
    let mut line = String::new();
    let read = io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    if read == 0 {
        return Ok(None);
    }
    while line.ends_with('\n') || line.ends_with('\r') {
        line.pop();
    }
    Ok(Some(line))
}
