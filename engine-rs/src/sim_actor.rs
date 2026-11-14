// Deterministic simulation runtime (virtual clock, seeded PRNG, fault injection, trace log)
// and message-passing supervised actors built strictly on Rust std primitives.

use std::{
    cell::RefCell,
    cmp::Ordering as CmpOrdering,
    collections::{BTreeMap, BinaryHeap, HashMap},
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream, ToSocketAddrs},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use serde_json::Value as JsonValue;

use crate::bytecode::Op;
use crate::runtime::resolve_function_object;
use crate::value::{Val, json_to_val, map_lookup, val_to_json};
use crate::vm::Vm;

#[derive(Clone, Debug)]
struct SimEvent {
    deliver_tick: u64,
    seq: u64,
    from: String,
    to: String,
    payload: Val,
}

impl PartialEq for SimEvent {
    fn eq(&self, other: &Self) -> bool {
        self.deliver_tick == other.deliver_tick && self.seq == other.seq
    }
}

impl Eq for SimEvent {}

impl PartialOrd for SimEvent {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}

impl Ord for SimEvent {
    fn cmp(&self, other: &Self) -> CmpOrdering {
        // Min-heap: earliest (deliver_tick, seq) pops first.
        (other.deliver_tick, other.seq).cmp(&(self.deliver_tick, self.seq))
    }
}

struct SimActorEntry {
    handler: Val,
    arity: usize,
    state: Val,
    vm: Vm,
    inbox_count: usize,
    processed: usize,
}

impl Clone for SimActorEntry {
    fn clone(&self) -> Self {
        Self {
            handler: self.handler.clone(),
            arity: self.arity,
            state: self.state.clone(),
            vm: clone_worker_vm(&self.vm),
            inbox_count: self.inbox_count,
            processed: self.processed,
        }
    }
}

#[derive(Clone)]
pub(crate) struct SimState {
    pub(crate) rng_state: u64,
    pub(crate) clock_ms: f64,
    pub(crate) tick: u64,
    pub(crate) next_seq: u64,
    pub(crate) faults: HashMap<String, f64>,
    pub(crate) log: Vec<Val>,
    sim_actors: HashMap<String, SimActorEntry>,
    sim_queue: BinaryHeap<SimEvent>,
    partitions: HashMap<(String, String), f64>,
    sim_trace: Vec<Val>,
    vfs_synced: BTreeMap<String, String>,
    vfs_dirty: BTreeMap<String, Option<String>>,
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
            tick: 0,
            next_seq: 1,
            faults: HashMap::new(),
            log: Vec::new(),
            sim_actors: HashMap::new(),
            sim_queue: BinaryHeap::new(),
            partitions: HashMap::new(),
            sim_trace: Vec::new(),
            vfs_synced: BTreeMap::new(),
            vfs_dirty: BTreeMap::new(),
        }
    }

    fn trigger_fault_if_configured(&mut self, targets: &[&str]) -> Option<String> {
        for &target in targets {
            let rate = self.faults.get(target).copied().unwrap_or(0.0);
            if rate <= 0.0 {
                continue;
            }
            let draw = self.next_f64();
            let hit = rate >= 1.0 || draw < rate;
            if hit {
                self.push_event(
                    "fault",
                    vec![
                        (Val::Str("target".into()), Val::Str(target.into())),
                        (Val::Str("triggered".into()), Val::Bool(true)),
                    ],
                );
                return Some(format!("SimulatedFault: {target}"));
            }
        }
        None
    }

    fn vfs_effective(&self, norm: &str) -> Option<&String> {
        if let Some(opt) = self.vfs_dirty.get(norm) {
            opt.as_ref()
        } else {
            self.vfs_synced.get(norm)
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

    fn push_cluster_trace(&mut self, tick: u64, kind: &str, pairs: Vec<(Val, Val)>) {
        let mut entry = Vec::with_capacity(pairs.len() + 2);
        entry.push((Val::Str("tick".into()), Val::Num(tick as f64)));
        entry.push((Val::Str("kind".into()), Val::Str(kind.into())));
        entry.extend(pairs);
        self.sim_trace.push(Val::Map(entry));
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
// Deterministic Virtual-Clock Actor Cluster (Event-Driven Replay)
// =========================================================================

fn clone_worker_vm(vm: &Vm) -> Vm {
    let mut worker_vm = Vm::new(vec![Op::Halt]);
    worker_vm.vars = vm.vars.clone();
    worker_vm.funcs = vm.funcs.clone();
    worker_vm.classes = vm.classes.clone();
    worker_vm.objects = vm.objects.clone();
    worker_vm.class_objects = vm.class_objects.clone();
    worker_vm.gc = vm.gc.clone();
    worker_vm
}

fn apply_actor_ret(arity: usize, state: &mut Val, ret: Val) -> Val {
    if arity >= 2 {
        match &ret {
            Val::Map(pairs) => {
                if let Some(ns) = map_lookup(pairs, &Val::Str("state".into())) {
                    *state = ns.clone();
                }
                if let Some(rep) = map_lookup(pairs, &Val::Str("reply".into())) {
                    rep.clone()
                } else {
                    ret
                }
            }
            _ => {
                *state = ret.clone();
                ret
            }
        }
    } else {
        ret
    }
}

fn canonical_pair(a: &str, b: &str) -> (String, String) {
    if a <= b {
        (a.to_string(), b.to_string())
    } else {
        (b.to_string(), a.to_string())
    }
}

fn extract_init_state(opt: Option<&Val>) -> Result<(Val, usize), String> {
    let mut init_state = Val::Null;
    let mut max_restarts: usize = 3;
    if let Some(opt_val) = opt {
        match opt_val {
            Val::Map(pairs) => {
                let has_opt_key = map_lookup(pairs, &Val::Str("init".into())).is_some()
                    || map_lookup(pairs, &Val::Str("max_restarts".into())).is_some()
                    || map_lookup(pairs, &Val::Str("handler".into())).is_some()
                    || map_lookup(pairs, &Val::Str("name".into())).is_some();
                if has_opt_key {
                    if let Some(v) = map_lookup(pairs, &Val::Str("init".into())) {
                        init_state = v.clone();
                    }
                    if let Some(Val::Num(n)) = map_lookup(pairs, &Val::Str("max_restarts".into())) {
                        if *n < 0.0 || !n.is_finite() {
                            return Err("max_restarts must be non-negative integer".into());
                        }
                        max_restarts = *n as usize;
                    }
                } else {
                    init_state = opt_val.clone();
                }
            }
            Val::Null => {}
            other => init_state = other.clone(),
        }
    }
    Ok((init_state, max_restarts))
}

pub(crate) fn sim_actor_spawn(
    vm: &Vm,
    name: Option<&str>,
    handler: Val,
    opts: Option<&Val>,
) -> Result<Val, String> {
    let arity = resolve_handler_arity(vm, &handler)?;
    let (init_state, _) = extract_init_state(opts)?;
    let arc = require_sim()?;
    let mut st = arc.lock().map_err(|_| "simulation state poisoned")?;
    let actor_name = match name {
        Some(n) if !n.trim().is_empty() => n.to_string(),
        Some(_) => return Err("sim_actor_spawn name cannot be empty".into()),
        None => {
            let idx = st.sim_actors.len() + 1;
            format!("sim_actor_{idx}")
        }
    };
    let tick = st.tick;
    st.sim_actors.insert(
        actor_name.clone(),
        SimActorEntry {
            handler,
            arity,
            state: init_state,
            vm: clone_worker_vm(vm),
            inbox_count: 0,
            processed: 0,
        },
    );
    st.push_cluster_trace(
        tick,
        "spawn",
        vec![(Val::Str("actor".into()), Val::Str(actor_name.clone()))],
    );
    Ok(Val::Str(actor_name))
}

pub(crate) fn sim_actor_send(
    from: &str,
    to: &str,
    payload: Val,
    delay_ticks: u64,
) -> Result<Val, String> {
    let arc = require_sim()?;
    let mut st = arc.lock().map_err(|_| "simulation state poisoned")?;
    if !st.sim_actors.contains_key(to) {
        return Err(format!("sim_actor_send unknown actor '{to}'"));
    }
    let key = canonical_pair(from, to);
    let base_tick = st.tick.max(st.clock_ms.max(0.0) as u64);
    let deliver_tick = base_tick.saturating_add(delay_ticks);
    let seq = st.next_seq;
    st.next_seq = st.next_seq.saturating_add(1);

    let drop_prob = st.partitions.get(&key).copied().unwrap_or(0.0);
    if drop_prob > 0.0 && (drop_prob >= 1.0 || st.next_f64() < drop_prob) {
        st.push_cluster_trace(
            deliver_tick,
            "drop",
            vec![
                (Val::Str("seq".into()), Val::Num(seq as f64)),
                (Val::Str("from".into()), Val::Str(from.into())),
                (Val::Str("to".into()), Val::Str(to.into())),
                (Val::Str("payload".into()), payload),
            ],
        );
        return Ok(Val::Bool(false));
    }

    if let Some(target) = st.sim_actors.get_mut(to) {
        target.inbox_count = target.inbox_count.saturating_add(1);
    }
    st.push_cluster_trace(
        deliver_tick,
        "enqueue",
        vec![
            (Val::Str("seq".into()), Val::Num(seq as f64)),
            (Val::Str("from".into()), Val::Str(from.into())),
            (Val::Str("to".into()), Val::Str(to.into())),
            (Val::Str("payload".into()), payload.clone()),
        ],
    );
    st.sim_queue.push(SimEvent {
        deliver_tick,
        seq,
        from: from.to_string(),
        to: to.to_string(),
        payload,
    });
    Ok(Val::Bool(true))
}

pub(crate) fn sim_step() -> Result<Val, String> {
    let arc = require_sim()?;
    let (ev, handler, arity, mut state, mut exec_vm) = {
        let mut st = arc.lock().map_err(|_| "simulation state poisoned")?;
        let Some(ev) = st.sim_queue.pop() else {
            return Ok(Val::Null);
        };
        st.tick = st.tick.max(ev.deliver_tick);
        st.clock_ms = st.tick as f64;
        let entry = st
            .sim_actors
            .get_mut(&ev.to)
            .ok_or_else(|| format!("sim_step missing target actor '{}'", ev.to))?;
        entry.inbox_count = entry.inbox_count.saturating_sub(1);
        let h = entry.handler.clone();
        let a = entry.arity;
        let s = entry.state.clone();
        let vm = clone_worker_vm(&entry.vm);
        (ev, h, a, s, vm)
    };

    let args = if arity >= 2 {
        vec![ev.payload.clone(), state.clone()]
    } else {
        vec![ev.payload.clone()]
    };
    let ret = exec_vm.invoke_callable_sync(&handler, args);
    match ret {
        Ok(raw_reply) => {
            let reply_val = apply_actor_ret(arity, &mut state, raw_reply);
            let mut st = arc.lock().map_err(|_| "simulation state poisoned")?;
            if let Some(entry) = st.sim_actors.get_mut(&ev.to) {
                entry.state = state.clone();
                entry.processed += 1;
            }
            st.push_cluster_trace(
                ev.deliver_tick,
                "deliver",
                vec![
                    (Val::Str("seq".into()), Val::Num(ev.seq as f64)),
                    (Val::Str("from".into()), Val::Str(ev.from.clone())),
                    (Val::Str("to".into()), Val::Str(ev.to.clone())),
                    (Val::Str("payload".into()), ev.payload),
                    (Val::Str("reply".into()), reply_val.clone()),
                ],
            );
            Ok(Val::Map(vec![
                (Val::Str("tick".into()), Val::Num(ev.deliver_tick as f64)),
                (Val::Str("seq".into()), Val::Num(ev.seq as f64)),
                (Val::Str("from".into()), Val::Str(ev.from)),
                (Val::Str("to".into()), Val::Str(ev.to)),
                (Val::Str("reply".into()), reply_val),
                (Val::Str("state".into()), state),
            ]))
        }
        Err(err) => {
            if let Ok(mut st) = arc.lock() {
                st.push_cluster_trace(
                    ev.deliver_tick,
                    "error",
                    vec![
                        (Val::Str("seq".into()), Val::Num(ev.seq as f64)),
                        (Val::Str("to".into()), Val::Str(ev.to)),
                        (Val::Str("error".into()), Val::Str(err.clone())),
                    ],
                );
            }
            Err(err)
        }
    }
}

pub(crate) fn sim_run_until(until_tick: Option<u64>) -> Result<Val, String> {
    let arc = require_sim()?;
    let mut processed = 0usize;
    let limit = 100_000usize;
    loop {
        if processed >= limit {
            return Err(format!("sim_run_until exceeded {limit} event steps"));
        }
        let next_tick = {
            let st = arc.lock().map_err(|_| "simulation state poisoned")?;
            st.sim_queue.peek().map(|ev| ev.deliver_tick)
        };
        let Some(t) = next_tick else {
            break;
        };
        if let Some(max_t) = until_tick
            && t > max_t
        {
            break;
        }
        let _ = sim_step()?;
        processed += 1;
    }
    let mut st = arc.lock().map_err(|_| "simulation state poisoned")?;
    if let Some(max_t) = until_tick {
        st.tick = st.tick.max(max_t);
        st.clock_ms = st.tick as f64;
    }
    Ok(Val::Map(vec![
        (Val::Str("tick".into()), Val::Num(st.tick as f64)),
        (Val::Str("processed".into()), Val::Num(processed as f64)),
        (
            Val::Str("remaining".into()),
            Val::Num(st.sim_queue.len() as f64),
        ),
    ]))
}

pub(crate) fn sim_partition(a: &str, b: &str, drop_prob: f64) -> Result<Val, String> {
    if !drop_prob.is_finite() || !(0.0..=1.0).contains(&drop_prob) {
        return Err("sim_partition drop_prob must be in [0.0, 1.0]".into());
    }
    let arc = require_sim()?;
    let mut st = arc.lock().map_err(|_| "simulation state poisoned")?;
    let key = canonical_pair(a, b);
    let tick = st.tick;
    if drop_prob == 0.0 {
        st.partitions.remove(&key);
        st.push_cluster_trace(
            tick,
            "heal",
            vec![
                (Val::Str("a".into()), Val::Str(key.0)),
                (Val::Str("b".into()), Val::Str(key.1)),
            ],
        );
    } else {
        st.partitions.insert(key.clone(), drop_prob);
        st.push_cluster_trace(
            tick,
            "partition",
            vec![
                (Val::Str("a".into()), Val::Str(key.0)),
                (Val::Str("b".into()), Val::Str(key.1)),
                (Val::Str("drop_prob".into()), Val::Num(drop_prob)),
            ],
        );
    }
    Ok(Val::Bool(true))
}

pub(crate) fn sim_actor_state(name: &str) -> Result<Val, String> {
    let arc = require_sim()?;
    let st = arc.lock().map_err(|_| "simulation state poisoned")?;
    st.sim_actors
        .get(name)
        .map(|entry| entry.state.clone())
        .ok_or_else(|| format!("unknown simulated actor '{name}'"))
}

pub(crate) fn sim_trace() -> Result<Val, String> {
    let arc = require_sim()?;
    let st = arc.lock().map_err(|_| "simulation state poisoned")?;
    Ok(Val::List(st.sim_trace.clone()))
}

// =========================================================================
// Message-Passing Supervised Actors & OTP Supervisor Trees
// =========================================================================

enum ActorMsg {
    Envelope {
        payload: Val,
        reply: Option<mpsc::Sender<Result<Val, String>>>,
    },
    ResetState(Option<mpsc::Sender<()>>),
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
    pending: Arc<AtomicUsize>,
}

#[derive(Clone)]
struct SupervisorHook {
    sup: Arc<Mutex<SupervisorState>>,
    child_idx: usize,
}

struct SupervisedChild {
    id: u64,
    name: String,
}

struct SupervisorState {
    id: u64,
    strategy: String,
    status: String,
    restarts: usize,
    max_restarts: usize,
    children: Vec<SupervisedChild>,
}

static ACTOR_IDS: AtomicU64 = AtomicU64::new(1);
static ACTORS: OnceLock<Mutex<HashMap<u64, Arc<Mutex<ActorHandle>>>>> = OnceLock::new();
static SUP_IDS: AtomicU64 = AtomicU64::new(1);
static SUPERVISORS: OnceLock<Mutex<HashMap<u64, Arc<Mutex<SupervisorState>>>>> = OnceLock::new();

fn actors() -> &'static Mutex<HashMap<u64, Arc<Mutex<ActorHandle>>>> {
    ACTORS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn supervisors() -> &'static Mutex<HashMap<u64, Arc<Mutex<SupervisorState>>>> {
    SUPERVISORS.get_or_init(|| Mutex::new(HashMap::new()))
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

fn supervisor_on_child_crash(hook: &SupervisorHook) -> bool {
    let (affected_ids, escalated, all_ids) = {
        let Ok(mut sup) = hook.sup.lock() else {
            return false;
        };
        sup.restarts = sup.restarts.saturating_add(1);
        if sup.restarts > sup.max_restarts {
            sup.status = "escalated".into();
            let all = sup.children.iter().map(|c| c.id).collect::<Vec<_>>();
            (Vec::new(), true, all)
        } else {
            let n = sup.children.len();
            let indices: Vec<usize> = match sup.strategy.as_str() {
                "one_for_one" => Vec::new(),
                "one_for_all" => (0..n).filter(|&i| i != hook.child_idx).collect(),
                "rest_for_one" => ((hook.child_idx + 1)..n).collect(),
                _ => Vec::new(),
            };
            let ids = indices.into_iter().map(|i| sup.children[i].id).collect();
            (ids, false, Vec::new())
        }
    };

    if escalated {
        for cid in all_ids {
            if cid != hook_child_id(hook) {
                let _ = actor_stop(cid);
            }
        }
        return true;
    }

    for sib_id in affected_ids {
        if let Ok(tx) = actor_sender(sib_id) {
            let (ack_tx, ack_rx) = mpsc::channel();
            if tx.send(ActorMsg::ResetState(Some(ack_tx))).is_ok() {
                let _ = ack_rx.recv_timeout(Duration::from_millis(500));
            }
        }
    }
    false
}

fn hook_child_id(hook: &SupervisorHook) -> u64 {
    hook.sup
        .lock()
        .ok()
        .and_then(|s| s.children.get(hook.child_idx).map(|c| c.id))
        .unwrap_or(0)
}

fn spawn_actor_internal(
    vm: &Vm,
    handler: Val,
    init_state: Val,
    max_restarts: usize,
    sup_hook: Option<SupervisorHook>,
) -> Result<u64, String> {
    let arity = resolve_handler_arity(vm, &handler)?;
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
    let pending = Arc::new(AtomicUsize::new(0));

    let worker_status = status.clone();
    let worker_pending = pending.clone();
    let mut worker_vm = clone_worker_vm(vm);
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
                ActorMsg::ResetState(ack) => {
                    state = init_state.clone();
                    if let Ok(mut st) = worker_status.lock() {
                        st.restarts = st.restarts.saturating_add(1);
                        st.status = "restarted".into();
                    }
                    if let Some(ack_tx) = ack {
                        let _ = ack_tx.send(());
                    }
                }
                ActorMsg::Envelope { payload, reply } => {
                    worker_pending.fetch_sub(1, Ordering::SeqCst);
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
                            let reply_val = apply_actor_ret(arity, &mut state, ret);
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
                            if let Some(ref hook) = sup_hook
                                && supervisor_on_child_crash(hook)
                            {
                                fatal = true;
                                if let Ok(mut st) = worker_status.lock() {
                                    st.alive = false;
                                    st.status = "failed".into();
                                }
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
        pending,
    }));
    actors()
        .lock()
        .map_err(|_| "actor registry poisoned")?
        .insert(id, handle);
    Ok(id)
}

pub(crate) fn actor_spawn(vm: &Vm, handler: Val, opts: Option<&Val>) -> Result<Val, String> {
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
    let id = spawn_actor_internal(vm, handler, init_state, max_restarts, None)?;
    Ok(Val::Num(id as f64))
}

fn actor_entry(id: u64) -> Result<(mpsc::Sender<ActorMsg>, Arc<AtomicUsize>), String> {
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
    let tx = guard
        .tx
        .clone()
        .ok_or_else(|| format!("actor {id} mailbox closed"))?;
    Ok((tx, guard.pending.clone()))
}

fn actor_sender(id: u64) -> Result<mpsc::Sender<ActorMsg>, String> {
    Ok(actor_entry(id)?.0)
}

pub(crate) fn actor_send(id: u64, payload: Val) -> Result<Val, String> {
    if sim_should_fault("actor.send") {
        return Err("SimulatedFault: actor.send".into());
    }
    let (tx, pending) = actor_entry(id)?;
    pending.fetch_add(1, Ordering::SeqCst);
    if tx
        .send(ActorMsg::Envelope {
            payload,
            reply: None,
        })
        .is_err()
    {
        pending.fetch_sub(1, Ordering::SeqCst);
        return Err(format!("actor {id} mailbox closed"));
    }
    Ok(Val::Bool(true))
}

pub(crate) fn actor_ask(id: u64, payload: Val, timeout_ms: u64) -> Result<Val, String> {
    if sim_should_fault("actor.ask") {
        return Err("SimulatedFault: actor.ask".into());
    }
    let (tx, pending) = actor_entry(id)?;
    let (rtx, rrx) = mpsc::channel();
    pending.fetch_add(1, Ordering::SeqCst);
    if tx
        .send(ActorMsg::Envelope {
            payload,
            reply: Some(rtx),
        })
        .is_err()
    {
        pending.fetch_sub(1, Ordering::SeqCst);
        return Err(format!("actor {id} mailbox closed"));
    }
    rrx.recv_timeout(Duration::from_millis(timeout_ms))
        .map_err(|_| format!("actor_ask timed out after {timeout_ms}ms for actor {id}"))?
}

pub(crate) fn actor_mailbox_len(id: u64) -> Result<Val, String> {
    let handle = {
        let map = actors().lock().map_err(|_| "actor registry poisoned")?;
        map.get(&id)
            .cloned()
            .ok_or_else(|| format!("unknown actor id {id}"))?
    };
    let guard = handle.lock().map_err(|_| "actor handle poisoned")?;
    Ok(Val::Num(guard.pending.load(Ordering::SeqCst) as f64))
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
    let mailbox_len = guard.pending.load(Ordering::SeqCst);
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
        (Val::Str("mailbox_len".into()), Val::Num(mailbox_len as f64)),
        (
            Val::Str("last_error".into()),
            match &st.last_error {
                Some(e) => Val::Str(e.clone()),
                None => Val::Null,
            },
        ),
    ]))
}

pub(crate) fn supervisor_spawn(
    vm: &Vm,
    strategy: &str,
    specs: &[Val],
    opts: Option<&Val>,
) -> Result<Val, String> {
    if !matches!(strategy, "one_for_one" | "one_for_all" | "rest_for_one") {
        return Err(format!(
            "unknown supervisor strategy '{strategy}'; expected 'one_for_one', 'one_for_all', or 'rest_for_one'"
        ));
    }
    if specs.is_empty() {
        return Err("supervisor_spawn requires at least one child spec".into());
    }
    let sup_max_restarts = match opts {
        Some(Val::Map(pairs)) => match map_lookup(pairs, &Val::Str("max_restarts".into())) {
            Some(Val::Num(n)) if n.is_finite() && *n >= 0.0 => *n as usize,
            Some(_) => {
                return Err("supervisor_spawn max_restarts must be non-negative Number".into());
            }
            None => 10,
        },
        None | Some(Val::Null) => 10,
        _ => return Err("supervisor_spawn options must be Map".into()),
    };

    let sup_id = SUP_IDS.fetch_add(1, Ordering::Relaxed);
    let sup_arc = Arc::new(Mutex::new(SupervisorState {
        id: sup_id,
        strategy: strategy.to_string(),
        status: "running".to_string(),
        restarts: 0,
        max_restarts: sup_max_restarts,
        children: Vec::with_capacity(specs.len()),
    }));

    let mut spawned = Vec::with_capacity(specs.len());
    for (idx, spec) in specs.iter().enumerate() {
        let (name, handler, init_state, child_max_restarts) = match spec {
            Val::Function(_) => (format!("child_{idx}"), spec.clone(), Val::Null, 5usize),
            Val::Map(pairs) => {
                let h = map_lookup(pairs, &Val::Str("handler".into()))
                    .or_else(|| map_lookup(pairs, &Val::Str("fn".into())))
                    .ok_or_else(|| {
                        format!("supervisor child spec #{idx} missing 'handler' Function")
                    })?
                    .clone();
                let name = match map_lookup(pairs, &Val::Str("name".into())) {
                    Some(Val::Str(s)) => s.clone(),
                    _ => format!("child_{idx}"),
                };
                let init = map_lookup(pairs, &Val::Str("init".into()))
                    .cloned()
                    .unwrap_or(Val::Null);
                let mr = match map_lookup(pairs, &Val::Str("max_restarts".into())) {
                    Some(Val::Num(n)) if n.is_finite() && *n >= 0.0 => *n as usize,
                    Some(_) => {
                        return Err(format!("supervisor child spec #{idx} invalid max_restarts"));
                    }
                    None => 5usize,
                };
                (name, h, init, mr)
            }
            _ => {
                return Err(format!(
                    "supervisor child spec #{idx} must be Map or Function"
                ));
            }
        };
        let hook = SupervisorHook {
            sup: sup_arc.clone(),
            child_idx: idx,
        };
        match spawn_actor_internal(vm, handler, init_state, child_max_restarts, Some(hook)) {
            Ok(cid) => spawned.push(SupervisedChild { id: cid, name }),
            Err(err) => {
                for prev in spawned {
                    let _ = actor_stop(prev.id);
                }
                return Err(err);
            }
        }
    }

    if let Ok(mut sup) = sup_arc.lock() {
        sup.children = spawned;
    }
    supervisors()
        .lock()
        .map_err(|_| "supervisor registry poisoned")?
        .insert(sup_id, sup_arc);
    Ok(Val::Num(sup_id as f64))
}

pub(crate) fn supervisor_status(sup_id: u64) -> Result<Val, String> {
    let sup_arc = {
        let map = supervisors()
            .lock()
            .map_err(|_| "supervisor registry poisoned")?;
        map.get(&sup_id)
            .cloned()
            .ok_or_else(|| format!("unknown supervisor id {sup_id}"))?
    };
    let (id, strategy, status, restarts, max_restarts, children_meta) = {
        let s = sup_arc.lock().map_err(|_| "supervisor state poisoned")?;
        let meta = s
            .children
            .iter()
            .map(|c| (c.id, c.name.clone()))
            .collect::<Vec<_>>();
        (
            s.id,
            s.strategy.clone(),
            s.status.clone(),
            s.restarts,
            s.max_restarts,
            meta,
        )
    };
    let mut child_ids = Vec::with_capacity(children_meta.len());
    let mut named_pairs = Vec::with_capacity(children_meta.len());
    let mut child_statuses = Vec::with_capacity(children_meta.len());
    for (cid, cname) in children_meta {
        child_ids.push(Val::Num(cid as f64));
        named_pairs.push((Val::Str(cname.clone()), Val::Num(cid as f64)));
        let st_map = actor_status(cid)?;
        if let Val::Map(mut pairs) = st_map {
            pairs.push((Val::Str("name".into()), Val::Str(cname)));
            child_statuses.push(Val::Map(pairs));
        } else {
            child_statuses.push(st_map);
        }
    }
    Ok(Val::Map(vec![
        (Val::Str("id".into()), Val::Num(id as f64)),
        (Val::Str("strategy".into()), Val::Str(strategy)),
        (Val::Str("status".into()), Val::Str(status)),
        (Val::Str("restarts".into()), Val::Num(restarts as f64)),
        (
            Val::Str("max_restarts".into()),
            Val::Num(max_restarts as f64),
        ),
        (Val::Str("child_ids".into()), Val::List(child_ids)),
        (Val::Str("named".into()), Val::Map(named_pairs)),
        (Val::Str("children".into()), Val::List(child_statuses)),
    ]))
}

// =========================================================================
// Deterministic Virtual Fault-Injectable Filesystem (Crash + Replay Safe)
// =========================================================================

fn vfs_norm_path(raw: &str) -> Result<String, String> {
    let replaced = raw.replace('\\', "/");
    let mut parts = Vec::new();
    for seg in replaced.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            return Err(format!("sim_fs invalid path traversal in '{raw}'"));
        }
        parts.push(seg);
    }
    if parts.is_empty() {
        return Err("sim_fs path must not be empty".into());
    }
    Ok(format!("/{}", parts.join("/")))
}

fn vfs_map_to_val(entries: impl Iterator<Item = (String, String)>) -> Val {
    Val::Map(entries.map(|(k, v)| (Val::Str(k), Val::Str(v))).collect())
}

pub(crate) fn sim_fs_write(path: &str, data: &str) -> Result<Val, String> {
    let norm = vfs_norm_path(path)?;
    let arc = require_sim()?;
    let mut st = arc.lock().map_err(|_| "simulation state poisoned")?;
    if let Some(err) = st.trigger_fault_if_configured(&["fs.write", "fs"]) {
        return Err(err);
    }
    st.vfs_dirty.insert(norm.clone(), Some(data.to_string()));
    st.push_event(
        "fs_write",
        vec![
            (Val::Str("path".into()), Val::Str(norm)),
            (Val::Str("bytes".into()), Val::Num(data.len() as f64)),
        ],
    );
    Ok(Val::Num(data.len() as f64))
}

pub(crate) fn sim_fs_read(path: &str) -> Result<Val, String> {
    let norm = vfs_norm_path(path)?;
    let arc = require_sim()?;
    let mut st = arc.lock().map_err(|_| "simulation state poisoned")?;
    if let Some(err) = st.trigger_fault_if_configured(&["fs.read", "fs"]) {
        return Err(err);
    }
    match st.vfs_effective(&norm) {
        Some(content) => {
            let out = content.clone();
            st.push_event("fs_read", vec![(Val::Str("path".into()), Val::Str(norm))]);
            Ok(Val::Str(out))
        }
        None => Err(format!("sim_fs_read: file '{norm}' not found")),
    }
}

pub(crate) fn sim_fs_delete(path: &str) -> Result<Val, String> {
    let norm = vfs_norm_path(path)?;
    let arc = require_sim()?;
    let mut st = arc.lock().map_err(|_| "simulation state poisoned")?;
    if let Some(err) = st.trigger_fault_if_configured(&["fs.delete", "fs"]) {
        return Err(err);
    }
    let existed = st.vfs_effective(&norm).is_some();
    if existed {
        if st.vfs_synced.contains_key(&norm) {
            st.vfs_dirty.insert(norm.clone(), None);
        } else {
            st.vfs_dirty.remove(&norm);
        }
    }
    st.push_event(
        "fs_delete",
        vec![
            (Val::Str("path".into()), Val::Str(norm)),
            (Val::Str("existed".into()), Val::Bool(existed)),
        ],
    );
    Ok(Val::Bool(existed))
}

pub(crate) fn sim_fs_list(prefix: Option<&str>) -> Result<Val, String> {
    let norm_prefix = match prefix {
        Some(p) if !p.trim().is_empty() && p.trim() != "/" => Some(vfs_norm_path(p)?),
        _ => None,
    };
    let arc = require_sim()?;
    let st = arc.lock().map_err(|_| "simulation state poisoned")?;
    let mut keys: Vec<String> = st
        .vfs_synced
        .keys()
        .chain(st.vfs_dirty.keys())
        .cloned()
        .collect();
    keys.sort();
    keys.dedup();
    let out = keys
        .into_iter()
        .filter(|k| st.vfs_effective(k).is_some())
        .filter(|k| {
            norm_prefix
                .as_ref()
                .is_none_or(|p| k == p || k.starts_with(&format!("{p}/")))
        })
        .map(Val::Str)
        .collect();
    Ok(Val::List(out))
}

pub(crate) fn sim_fs_sync() -> Result<Val, String> {
    let arc = require_sim()?;
    let mut st = arc.lock().map_err(|_| "simulation state poisoned")?;
    if let Some(err) = st.trigger_fault_if_configured(&["fs.sync", "fs"]) {
        return Err(err);
    }
    let dirty = std::mem::take(&mut st.vfs_dirty);
    let count = dirty.len();
    for (k, opt) in dirty {
        match opt {
            Some(content) => {
                st.vfs_synced.insert(k, content);
            }
            None => {
                st.vfs_synced.remove(&k);
            }
        }
    }
    st.push_event(
        "fs_sync",
        vec![(Val::Str("flushed".into()), Val::Num(count as f64))],
    );
    Ok(Val::Num(count as f64))
}

pub(crate) fn sim_fs_crash() -> Result<Val, String> {
    let arc = require_sim()?;
    let mut st = arc.lock().map_err(|_| "simulation state poisoned")?;
    let lost = st.vfs_dirty.len();
    st.vfs_dirty.clear();
    st.push_event(
        "fs_crash",
        vec![(Val::Str("lost_dirty".into()), Val::Num(lost as f64))],
    );
    Ok(Val::Map(vec![
        (Val::Str("lost_dirty".into()), Val::Num(lost as f64)),
        (
            Val::Str("synced_files".into()),
            Val::Num(st.vfs_synced.len() as f64),
        ),
    ]))
}

pub(crate) fn sim_fs_snapshot() -> Result<Val, String> {
    let arc = require_sim()?;
    let st = arc.lock().map_err(|_| "simulation state poisoned")?;
    let synced = vfs_map_to_val(st.vfs_synced.iter().map(|(k, v)| (k.clone(), v.clone())));
    let dirty = Val::Map(
        st.vfs_dirty
            .iter()
            .map(|(k, opt)| {
                let val = match opt {
                    Some(s) => Val::Str(s.clone()),
                    None => Val::Null,
                };
                (Val::Str(k.clone()), val)
            })
            .collect(),
    );
    let mut eff_keys: Vec<String> = st
        .vfs_synced
        .keys()
        .chain(st.vfs_dirty.keys())
        .cloned()
        .collect();
    eff_keys.sort();
    eff_keys.dedup();
    let effective = vfs_map_to_val(
        eff_keys
            .into_iter()
            .filter_map(|k| st.vfs_effective(&k).map(|v| (k, v.clone()))),
    );
    Ok(Val::Map(vec![
        (Val::Str("synced".into()), synced),
        (Val::Str("dirty".into()), dirty),
        (Val::Str("effective".into()), effective),
        (Val::Str("tick".into()), Val::Num(st.tick as f64)),
    ]))
}

// =========================================================================
// Location-Transparent Distributed TCP Actor Nodes
// =========================================================================

struct ActorNodeHandle {
    addr: String,
    running: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

static NODE_IDS: AtomicU64 = AtomicU64::new(1);
static ACTOR_NODES: OnceLock<Mutex<HashMap<u64, ActorNodeHandle>>> = OnceLock::new();

fn actor_nodes() -> &'static Mutex<HashMap<u64, ActorNodeHandle>> {
    ACTOR_NODES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn write_json_line(stream: &mut TcpStream, body: &JsonValue) -> Result<(), String> {
    let mut line = serde_json::to_string(body).map_err(|e| e.to_string())?;
    line.push('\n');
    stream
        .write_all(line.as_bytes())
        .map_err(|e| e.to_string())?;
    stream.flush().map_err(|e| e.to_string())
}

fn handle_actor_node_conn(mut stream: TcpStream, actor_id: u64) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(15)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(15)));
    let reader_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut reader = BufReader::new(reader_stream);
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) | Err(_) => return,
        Ok(_) => {}
    }
    let resp = match serde_json::from_str::<JsonValue>(line.trim()) {
        Ok(req) => {
            let op = req.get("op").and_then(JsonValue::as_str).unwrap_or("ask");
            let timeout = req
                .get("timeout_ms")
                .and_then(JsonValue::as_u64)
                .unwrap_or(10_000);
            let payload = req.get("payload").map(json_to_val).unwrap_or(Val::Null);
            let res = match op {
                "send" => actor_send(actor_id, payload),
                "ask" => actor_ask(actor_id, payload, timeout),
                other => Err(format!("unknown remote actor op '{other}'")),
            };
            match res {
                Ok(v) => serde_json::json!({ "ok": val_to_json(&v) }),
                Err(e) => serde_json::json!({ "err": e }),
            }
        }
        Err(e) => serde_json::json!({ "err": format!("invalid actor RPC JSON: {e}") }),
    };
    let _ = write_json_line(&mut stream, &resp);
}

pub(crate) fn actor_node_listen(actor_id: u64, bind: Option<&Val>) -> Result<Val, String> {
    let _ = actor_entry(actor_id)?;
    let bind_addr = match bind {
        None | Some(Val::Null) => "127.0.0.1:0".to_string(),
        Some(Val::Num(port)) if port.is_finite() && *port >= 0.0 && *port <= 65535.0 => {
            format!("127.0.0.1:{}", *port as u16)
        }
        Some(Val::Str(s)) if !s.trim().is_empty() => s.trim().to_string(),
        Some(_) => {
            return Err(
                "actor_node_listen second argument must be port Number or bind String".into(),
            );
        }
    };
    let listener = TcpListener::bind(&bind_addr)
        .map_err(|e| format!("actor_node_listen failed to bind '{bind_addr}': {e}"))?;
    let local_addr = listener.local_addr().map_err(|e| e.to_string())?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("actor_node_listen set_nonblocking failed: {e}"))?;
    let addr_str = local_addr.to_string();
    let port = local_addr.port();
    let node_id = NODE_IDS.fetch_add(1, Ordering::SeqCst);
    let running = Arc::new(AtomicBool::new(true));
    let running_thr = running.clone();
    let join = thread::spawn(move || {
        while running_thr.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = stream.set_nonblocking(false);
                    thread::spawn(move || handle_actor_node_conn(stream, actor_id));
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });
    let mut map = actor_nodes()
        .lock()
        .map_err(|_| "actor node registry poisoned")?;
    map.insert(
        node_id,
        ActorNodeHandle {
            addr: addr_str.clone(),
            running,
            join: Some(join),
        },
    );
    Ok(Val::Map(vec![
        (Val::Str("node_id".into()), Val::Num(node_id as f64)),
        (Val::Str("actor_id".into()), Val::Num(actor_id as f64)),
        (Val::Str("addr".into()), Val::Str(addr_str)),
        (Val::Str("port".into()), Val::Num(port as f64)),
    ]))
}

pub(crate) fn actor_node_stop(target: &Val) -> Result<Val, String> {
    let node_id = match target {
        Val::Num(n) if n.is_finite() && *n >= 0.0 => *n as u64,
        Val::Map(pairs) => match map_lookup(pairs, &Val::Str("node_id".into())) {
            Some(Val::Num(n)) if n.is_finite() && *n >= 0.0 => *n as u64,
            _ => return Err("actor_node_stop map missing numeric node_id".into()),
        },
        _ => return Err("actor_node_stop expects node_id Number or node Map".into()),
    };
    let handle = {
        let mut map = actor_nodes()
            .lock()
            .map_err(|_| "actor node registry poisoned")?;
        map.remove(&node_id)
            .ok_or_else(|| format!("unknown actor node id {node_id}"))?
    };
    handle.running.store(false, Ordering::SeqCst);
    let _ = &handle.addr;
    if let Some(j) = handle.join {
        let _ = j.join();
    }
    Ok(Val::Bool(true))
}

enum RemoteTarget {
    LocalActor(u64),
    TcpAddr(String),
}

fn resolve_remote_target(target: &Val) -> Result<RemoteTarget, String> {
    match target {
        Val::Num(n) if n.is_finite() && *n >= 0.0 => Ok(RemoteTarget::LocalActor(*n as u64)),
        Val::Str(s) => {
            if let Some(rest) = s.strip_prefix("local:") {
                let id = rest
                    .trim()
                    .parse::<u64>()
                    .map_err(|_| format!("invalid local actor ref '{s}'"))?;
                Ok(RemoteTarget::LocalActor(id))
            } else {
                Ok(RemoteTarget::TcpAddr(s.trim().to_string()))
            }
        }
        Val::Map(pairs) => {
            if let Some(Val::Str(addr)) = map_lookup(pairs, &Val::Str("addr".into())) {
                Ok(RemoteTarget::TcpAddr(addr.clone()))
            } else if let Some(Val::Num(id)) = map_lookup(pairs, &Val::Str("actor_id".into()))
                && id.is_finite()
                && *id >= 0.0
            {
                Ok(RemoteTarget::LocalActor(*id as u64))
            } else {
                Err("remote actor Map must contain 'addr' or 'actor_id'".into())
            }
        }
        _ => Err("remote actor target must be actor_id, 'host:port' String, or node Map".into()),
    }
}

fn actor_remote_rpc(addr: &str, op: &str, payload: &Val, timeout_ms: u64) -> Result<Val, String> {
    crate::runtime::require_cap("net")?;
    if sim_should_fault("net") || sim_should_fault(&format!("actor.remote.{op}")) {
        return Err(format!("SimulatedFault: actor.remote.{op}"));
    }
    let sock_addr = addr
        .to_socket_addrs()
        .map_err(|e| format!("failed to resolve actor addr '{addr}': {e}"))?
        .next()
        .ok_or_else(|| format!("no socket address resolved for '{addr}'"))?;
    let timeout = Duration::from_millis(timeout_ms.max(1));
    let mut stream = TcpStream::connect_timeout(&sock_addr, timeout)
        .map_err(|e| format!("remote actor connect '{addr}' failed: {e}"))?;
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    let req = serde_json::json!({
        "op": op,
        "payload": val_to_json(payload),
        "timeout_ms": timeout_ms,
    });
    write_json_line(&mut stream, &req)?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| format!("remote actor response read '{addr}' failed: {e}"))?;
    let parsed: JsonValue = serde_json::from_str(line.trim())
        .map_err(|e| format!("invalid remote actor response from '{addr}': {e}"))?;
    if let Some(err) = parsed.get("err").and_then(JsonValue::as_str) {
        return Err(err.to_string());
    }
    Ok(parsed.get("ok").map(json_to_val).unwrap_or(Val::Null))
}

pub(crate) fn actor_remote_send(target: &Val, payload: Val) -> Result<Val, String> {
    match resolve_remote_target(target)? {
        RemoteTarget::LocalActor(id) => actor_send(id, payload),
        RemoteTarget::TcpAddr(addr) => actor_remote_rpc(&addr, "send", &payload, 5_000),
    }
}

pub(crate) fn actor_remote_ask(target: &Val, payload: Val, timeout_ms: u64) -> Result<Val, String> {
    match resolve_remote_target(target)? {
        RemoteTarget::LocalActor(id) => actor_ask(id, payload, timeout_ms),
        RemoteTarget::TcpAddr(addr) => actor_remote_rpc(&addr, "ask", &payload, timeout_ms),
    }
}
