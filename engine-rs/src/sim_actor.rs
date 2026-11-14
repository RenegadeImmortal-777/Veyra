// Deterministic simulation runtime (virtual clock, seeded PRNG, fault injection, trace log)
// and message-passing supervised actors built strictly on Rust std primitives.

use std::{
    cell::RefCell,
    collections::HashMap,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use crate::bytecode::Op;
use crate::runtime::resolve_function_object;
use crate::value::{Val, map_lookup};
use crate::vm::Vm;

#[derive(Clone, Debug)]
pub(crate) struct SimState {
    pub(crate) rng_state: u64,
    pub(crate) clock_ms: f64,
    pub(crate) faults: HashMap<String, f64>,
    pub(crate) log: Vec<Val>,
}

impl SimState {
    fn new(seed: u64) -> Self {
        let initial = if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        };
        Self {
            rng_state: initial,
            clock_ms: 0.0,
            faults: HashMap::new(),
            log: Vec::new(),
        }
    }

    fn next_u64(&mut self) -> u64 {
        self.rng_state = self.rng_state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.rng_state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn next_f64(&mut self) -> f64 {
        let top53 = self.next_u64() >> 11;
        (top53 as f64) / ((1u64 << 53) as f64)
    }

    fn push_event(&mut self, event: &str, pairs: Vec<(Val, Val)>) -> Val {
        let mut entry = Vec::with_capacity(pairs.len() + 2);
        entry.push((Val::Str("t".into()), Val::Num(self.clock_ms)));
        entry.push((Val::Str("event".into()), Val::Str(event.into())));
        entry.extend(pairs);
        let val = Val::Map(entry);
        self.log.push(val.clone());
        val
    }
}

thread_local! {
    static ACTIVE_SIM: RefCell<Option<Arc<Mutex<SimState>>>> = const { RefCell::new(None) };
}

pub(crate) fn current_sim() -> Option<Arc<Mutex<SimState>>> {
    ACTIVE_SIM.with(|cell| cell.borrow().clone())
}

struct SimGuard {
    prev: Option<Arc<Mutex<SimState>>>,
}

impl SimGuard {
    fn enter(sim: Option<Arc<Mutex<SimState>>>) -> Self {
        let prev = ACTIVE_SIM.with(|cell| cell.borrow_mut().replace_with_opt(sim));
        Self { prev }
    }
}

impl Drop for SimGuard {
    fn drop(&mut self) {
        let prev = self.prev.take();
        ACTIVE_SIM.with(|cell| {
            *cell.borrow_mut() = prev;
        });
    }
}

trait ReplaceOpt<T> {
    fn replace_with_opt(&mut self, v: Option<T>) -> Option<T>;
}

impl<T> ReplaceOpt<T> for Option<T> {
    fn replace_with_opt(&mut self, v: Option<T>) -> Option<T> {
        std::mem::replace(self, v)
    }
}

fn require_sim() -> Result<Arc<Mutex<SimState>>, String> {
    current_sim().ok_or_else(|| "simulation not active; call sim_start(seed) first".to_string())
}

pub(crate) fn sim_start(seed: u64) -> Val {
    let st = Arc::new(Mutex::new(SimState::new(seed)));
    ACTIVE_SIM.with(|cell| {
        *cell.borrow_mut() = Some(st);
    });
    Val::Bool(true)
}

pub(crate) fn sim_stop() -> Val {
    let had = ACTIVE_SIM.with(|cell| cell.borrow_mut().take().is_some());
    Val::Bool(had)
}

pub(crate) fn sim_now() -> Result<Val, String> {
    let arc = require_sim()?;
    let st = arc.lock().map_err(|_| "simulation state poisoned")?;
    Ok(Val::Num(st.clock_ms))
}

pub(crate) fn sim_advance(ms: f64) -> Result<Val, String> {
    if !ms.is_finite() || ms < 0.0 {
        return Err("sim_advance expects finite non-negative milliseconds".into());
    }
    let arc = require_sim()?;
    let mut st = arc.lock().map_err(|_| "simulation state poisoned")?;
    st.clock_ms += ms;
    st.push_event("advance", vec![(Val::Str("ms".into()), Val::Num(ms))]);
    Ok(Val::Num(st.clock_ms))
}

pub(crate) fn sim_rand() -> Result<Val, String> {
    let arc = require_sim()?;
    let mut st = arc.lock().map_err(|_| "simulation state poisoned")?;
    Ok(Val::Num(st.next_f64()))
}

pub(crate) fn sim_fault_set(target: &str, rate: f64) -> Result<Val, String> {
    if target.trim().is_empty() {
        return Err("sim_fault target must be non-empty String".into());
    }
    if !rate.is_finite() || !(0.0..=1.0).contains(&rate) {
        return Err("sim_fault rate must be between 0.0 and 1.0".into());
    }
    let arc = require_sim()?;
    let mut st = arc.lock().map_err(|_| "simulation state poisoned")?;
    if rate == 0.0 {
        st.faults.remove(target);
    } else {
        st.faults.insert(target.to_string(), rate);
    }
    st.push_event(
        "fault_config",
        vec![
            (Val::Str("target".into()), Val::Str(target.into())),
            (Val::Str("rate".into()), Val::Num(rate)),
        ],
    );
    Ok(Val::Bool(true))
}

pub(crate) fn sim_fault_check(target: &str) -> Result<Val, String> {
    let arc = require_sim()?;
    let mut st = arc.lock().map_err(|_| "simulation state poisoned")?;
    let rate = st.faults.get(target).copied().unwrap_or(0.0);
    if rate <= 0.0 {
        return Ok(Val::Bool(false));
    }
    let draw = st.next_f64();
    let hit = rate >= 1.0 || draw < rate;
    st.push_event(
        "fault",
        vec![
            (Val::Str("target".into()), Val::Str(target.into())),
            (Val::Str("triggered".into()), Val::Bool(hit)),
        ],
    );
    Ok(Val::Bool(hit))
}

pub(crate) fn sim_should_fault(target: &str) -> bool {
    let Some(arc) = current_sim() else {
        return false;
    };
    let Ok(mut st) = arc.lock() else {
        return false;
    };
    let rate = st.faults.get(target).copied().unwrap_or(0.0);
    if rate <= 0.0 {
        return false;
    }
    let draw = st.next_f64();
    let hit = rate >= 1.0 || draw < rate;
    if hit {
        st.push_event(
            "fault",
            vec![
                (Val::Str("target".into()), Val::Str(target.into())),
                (Val::Str("triggered".into()), Val::Bool(true)),
            ],
        );
    }
    hit
}

pub(crate) fn sim_log_get() -> Result<Val, String> {
    let arc = require_sim()?;
    let st = arc.lock().map_err(|_| "simulation state poisoned")?;
    Ok(Val::List(st.log.clone()))
}

pub(crate) fn sim_log_append(entry: Val) -> Result<Val, String> {
    let arc = require_sim()?;
    let mut st = arc.lock().map_err(|_| "simulation state poisoned")?;
    st.push_event("log", vec![(Val::Str("data".into()), entry)]);
    Ok(Val::Num(st.log.len() as f64))
}

// =========================================================================
// Message-Passing Supervised Actors
// =========================================================================

enum ActorMsg {
    Envelope {
        payload: Val,
        reply: Option<mpsc::Sender<Result<Val, String>>>,
    },
    Stop,
}

#[derive(Clone)]
struct ActorStatus {
    id: u64,
    alive: bool,
    status: String,
    restarts: usize,
    max_restarts: usize,
    processed: usize,
    last_error: Option<String>,
}

struct ActorHandle {
    tx: Option<mpsc::Sender<ActorMsg>>,
    join: Option<thread::JoinHandle<()>>,
    status: Arc<Mutex<ActorStatus>>,
}

static ACTOR_IDS: AtomicU64 = AtomicU64::new(1);
static ACTORS: OnceLock<Mutex<HashMap<u64, Arc<Mutex<ActorHandle>>>>> = OnceLock::new();

fn actors() -> &'static Mutex<HashMap<u64, Arc<Mutex<ActorHandle>>>> {
    ACTORS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn resolve_handler_arity(vm: &Vm, handler: &Val) -> Result<usize, String> {
    match handler {
        Val::Function(name) => {
            if let Some(def) = vm
                .funcs
                .get(name)
                .cloned()
                .or_else(|| resolve_function_object(name).ok().flatten())
            {
                return Ok(def.params.len());
            }
            Err(format!("actor_spawn unknown function '{name}'"))
        }
        _ => Err("actor_spawn handler must be a Function".into()),
    }
}

pub(crate) fn actor_spawn(vm: &Vm, handler: Val, opts: Option<&Val>) -> Result<Val, String> {
    let arity = resolve_handler_arity(vm, &handler)?;
    let mut init_state = Val::Null;
    let mut max_restarts: usize = 3;
    if let Some(opt_val) = opts {
        match opt_val {
            Val::Map(pairs) => {
                if let Some(v) = map_lookup(pairs, &Val::Str("init".into())) {
                    init_state = v.clone();
                }
                if let Some(Val::Num(n)) = map_lookup(pairs, &Val::Str("max_restarts".into())) {
                    if *n < 0.0 || !n.is_finite() {
                        return Err("actor_spawn max_restarts must be non-negative integer".into());
                    }
                    max_restarts = *n as usize;
                }
            }
            Val::Null => {}
            _ => return Err("actor_spawn options must be Map".into()),
        }
    }

    let id = ACTOR_IDS.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = mpsc::channel::<ActorMsg>();
    let status = Arc::new(Mutex::new(ActorStatus {
        id,
        alive: true,
        status: "running".to_string(),
        restarts: 0,
        max_restarts,
        processed: 0,
        last_error: None,
    }));

    let worker_status = status.clone();
    let mut worker_vm = Vm::new(vec![Op::Halt]);
    worker_vm.vars = vm.vars.clone();
    worker_vm.funcs = vm.funcs.clone();
    worker_vm.classes = vm.classes.clone();
    worker_vm.objects = vm.objects.clone();
    worker_vm.class_objects = vm.class_objects.clone();
    worker_vm.gc = vm.gc.clone();

    let sim_ctx = current_sim();
    let join = thread::spawn(move || {
        let _sim_guard = SimGuard::enter(sim_ctx.clone());
        let mut state = init_state.clone();
        while let Ok(msg) = rx.recv() {
            match msg {
                ActorMsg::Stop => {
                    if let Ok(mut st) = worker_status.lock() {
                        st.alive = false;
                        st.status = "stopped".into();
                    }
                    break;
                }
                ActorMsg::Envelope { payload, reply } => {
                    let args = if arity >= 2 {
                        vec![payload, state.clone()]
                    } else {
                        vec![payload]
                    };
                    let outcome = catch_unwind(AssertUnwindSafe(|| {
                        worker_vm.invoke_callable_sync(&handler, args)
                    }));
                    let res = match outcome {
                        Ok(inner) => inner,
                        Err(_) => Err("actor panicked during message execution".to_string()),
                    };
                    match res {
                        Ok(ret) => {
                            let reply_val = if arity >= 2 {
                                match &ret {
                                    Val::Map(pairs) => {
                                        if let Some(ns) =
                                            map_lookup(pairs, &Val::Str("state".into()))
                                        {
                                            state = ns.clone();
                                        }
                                        if let Some(rep) =
                                            map_lookup(pairs, &Val::Str("reply".into()))
                                        {
                                            rep.clone()
                                        } else {
                                            ret.clone()
                                        }
                                    }
                                    _ => {
                                        state = ret.clone();
                                        ret
                                    }
                                }
                            } else {
                                ret
                            };
                            if let Ok(mut st) = worker_status.lock() {
                                st.processed += 1;
                            }
                            if let Some(rtx) = reply {
                                let _ = rtx.send(Ok(reply_val));
                            }
                        }
                        Err(err) => {
                            let mut fatal = false;
                            let mut msg_err = err.clone();
                            if let Ok(mut st) = worker_status.lock() {
                                st.restarts += 1;
                                st.last_error = Some(err.clone());
                                if st.restarts <= st.max_restarts {
                                    st.status = "restarted".into();
                                    state = init_state.clone();
                                    msg_err = format!(
                                        "ActorError: {err} (restarted {}/{})",
                                        st.restarts, st.max_restarts
                                    );
                                } else {
                                    st.alive = false;
                                    st.status = "failed".into();
                                    fatal = true;
                                    msg_err = format!(
                                        "ActorError: {err} (exceeded max_restarts={})",
                                        st.max_restarts
                                    );
                                }
                            }
                            if let Some(ref arc) = sim_ctx
                                && let Ok(mut s) = arc.lock()
                            {
                                s.push_event(
                                    "actor_fault",
                                    vec![
                                        (Val::Str("actor_id".into()), Val::Num(id as f64)),
                                        (Val::Str("error".into()), Val::Str(err)),
                                        (Val::Str("fatal".into()), Val::Bool(fatal)),
                                    ],
                                );
                            }
                            if let Some(rtx) = reply {
                                let _ = rtx.send(Err(msg_err));
                            }
                            if fatal {
                                break;
                            }
                        }
                    }
                }
            }
        }
        // Drain any remaining queued messages if loop exited unexpectedly.
        while let Ok(rem) = rx.try_recv() {
            if let ActorMsg::Envelope {
                reply: Some(rtx), ..
            } = rem
            {
                let _ = rtx.send(Err("ActorError: actor terminated".into()));
            }
        }
    });

    let handle = Arc::new(Mutex::new(ActorHandle {
        tx: Some(tx),
        join: Some(join),
        status,
    }));
    actors()
        .lock()
        .map_err(|_| "actor registry poisoned")?
        .insert(id, handle);
    Ok(Val::Num(id as f64))
}

fn actor_sender(id: u64) -> Result<mpsc::Sender<ActorMsg>, String> {
    let handle = {
        let map = actors().lock().map_err(|_| "actor registry poisoned")?;
        map.get(&id)
            .cloned()
            .ok_or_else(|| format!("unknown actor id {id}"))?
    };
    let guard = handle.lock().map_err(|_| "actor handle poisoned")?;
    let alive = guard.status.lock().map(|s| s.alive).unwrap_or(false);
    if !alive {
        return Err(format!("actor {id} is not running"));
    }
    guard
        .tx
        .clone()
        .ok_or_else(|| format!("actor {id} mailbox closed"))
}

pub(crate) fn actor_send(id: u64, payload: Val) -> Result<Val, String> {
    if sim_should_fault("actor.send") {
        return Err("SimulatedFault: actor.send".into());
    }
    let tx = actor_sender(id)?;
    tx.send(ActorMsg::Envelope {
        payload,
        reply: None,
    })
    .map_err(|_| format!("actor {id} mailbox closed"))?;
    Ok(Val::Bool(true))
}

pub(crate) fn actor_ask(id: u64, payload: Val, timeout_ms: u64) -> Result<Val, String> {
    if sim_should_fault("actor.ask") {
        return Err("SimulatedFault: actor.ask".into());
    }
    let tx = actor_sender(id)?;
    let (rtx, rrx) = mpsc::channel();
    tx.send(ActorMsg::Envelope {
        payload,
        reply: Some(rtx),
    })
    .map_err(|_| format!("actor {id} mailbox closed"))?;
    rrx.recv_timeout(Duration::from_millis(timeout_ms))
        .map_err(|_| format!("actor_ask timed out after {timeout_ms}ms for actor {id}"))?
}

pub(crate) fn actor_stop(id: u64) -> Result<Val, String> {
    let handle = {
        let map = actors().lock().map_err(|_| "actor registry poisoned")?;
        map.get(&id)
            .cloned()
            .ok_or_else(|| format!("unknown actor id {id}"))?
    };
    let join = {
        let mut guard = handle.lock().map_err(|_| "actor handle poisoned")?;
        if let Some(tx) = guard.tx.take() {
            let _ = tx.send(ActorMsg::Stop);
        }
        guard.join.take()
    };
    if let Some(j) = join {
        let _ = j.join();
    }
    if let Ok(guard) = handle.lock()
        && let Ok(mut st) = guard.status.lock()
    {
        st.alive = false;
        if st.status != "failed" {
            st.status = "stopped".into();
        }
    }
    Ok(Val::Bool(true))
}

pub(crate) fn actor_status(id: u64) -> Result<Val, String> {
    let handle = {
        let map = actors().lock().map_err(|_| "actor registry poisoned")?;
        map.get(&id)
            .cloned()
            .ok_or_else(|| format!("unknown actor id {id}"))?
    };
    let guard = handle.lock().map_err(|_| "actor handle poisoned")?;
    let st = guard.status.lock().map_err(|_| "actor status poisoned")?;
    Ok(Val::Map(vec![
        (Val::Str("id".into()), Val::Num(st.id as f64)),
        (Val::Str("alive".into()), Val::Bool(st.alive)),
        (Val::Str("status".into()), Val::Str(st.status.clone())),
        (Val::Str("restarts".into()), Val::Num(st.restarts as f64)),
        (
            Val::Str("max_restarts".into()),
            Val::Num(st.max_restarts as f64),
        ),
        (Val::Str("processed".into()), Val::Num(st.processed as f64)),
        (
            Val::Str("last_error".into()),
            match &st.last_error {
                Some(e) => Val::Str(e.clone()),
                None => Val::Null,
            },
        ),
    ]))
}
