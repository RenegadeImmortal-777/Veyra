// Debug Adapter Protocol (DAP) server and VM debug session controller.
//
// Communicates over stdio using JSON-RPC `Content-Length` framing when invoked
// via `veyra dap`, and wires line breakpoints, stepping, stack traces, scopes,
// variable inspection, and expression evaluation into `Vm`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::thread;

use serde_json::{Value as JsonValue, json};

use crate::bytecode::{ClassDef, FunctionDef, ObjectData};
use crate::compiler::CompiledDebug;
use crate::pipeline::{compile_eval_expr, compile_file_with_debug};
use crate::value::Val;
use crate::vm::Vm;

pub(crate) const DAP_TERMINATED_ERR: &str = "__dap_terminated__";

#[derive(Clone, Debug, Default)]
pub(crate) struct DebugMap {
    scopes: Vec<Vec<(usize, usize)>>,
    scope_local_names: Vec<Vec<String>>,
    edges: HashMap<(usize, usize, usize), usize>,
    origins: Vec<(PathBuf, usize)>,
    valid_lines: HashMap<String, Vec<usize>>,
}

impl DebugMap {
    pub(crate) fn from_compiled(root: CompiledDebug, origins: Vec<(PathBuf, usize)>) -> Self {
        let mut map = Self {
            scopes: Vec::new(),
            scope_local_names: Vec::new(),
            edges: HashMap::new(),
            origins,
            valid_lines: HashMap::new(),
        };
        let mut valid_sets: HashMap<String, HashSet<usize>> = HashMap::new();
        map.insert_scope(root, &mut valid_sets);
        for (k, set) in valid_sets {
            let mut lines: Vec<usize> = set.into_iter().collect();
            lines.sort_unstable();
            map.valid_lines.insert(k, lines);
        }
        map
    }

    fn insert_scope(
        &mut self,
        compiled: CompiledDebug,
        valid_sets: &mut HashMap<String, HashSet<usize>>,
    ) -> usize {
        let scope_id = self.scopes.len();
        for &(_, combined_line) in &compiled.lines {
            if let Some((path, orig_line)) = self.origin_for_combined(combined_line) {
                if orig_line > 0 {
                    valid_sets
                        .entry(normalize_path(&path))
                        .or_default()
                        .insert(orig_line);
                }
            }
        }
        self.scopes.push(compiled.lines);
        self.scope_local_names.push(compiled.local_names);
        for (def_ip, sub_idx, child) in compiled.children {
            let child_id = self.insert_scope(child, valid_sets);
            self.edges.insert((scope_id, def_ip, sub_idx), child_id);
        }
        scope_id
    }

    pub(crate) fn local_names(&self, scope_id: usize) -> &[String] {
        self.scope_local_names
            .get(scope_id)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub(crate) fn merge_frame_vars(
        &self,
        scope_id: usize,
        vars: &HashMap<String, Val>,
        cells: &HashMap<String, Arc<Mutex<Val>>>,
        locals: &[Val],
        local_defined: &[bool],
    ) -> HashMap<String, Val> {
        let mut merged = merge_vars(vars, cells);
        let names = self.local_names(scope_id);
        for (slot, val) in locals.iter().enumerate() {
            if !local_defined.is_empty() && !local_defined.get(slot).copied().unwrap_or(false) {
                continue;
            }
            if let Some(name) = names.get(slot) {
                if !name.is_empty() {
                    merged.insert(name.clone(), val.clone());
                }
            }
        }
        merged
    }

    fn origin_for_combined(&self, combined_line: usize) -> Option<(PathBuf, usize)> {
        if combined_line == 0 {
            return None;
        }
        self.origins.get(combined_line - 1).cloned()
    }

    pub(crate) fn child_scope(
        &self,
        parent_scope: usize,
        def_ip: usize,
        sub_idx: usize,
    ) -> Option<usize> {
        self.edges.get(&(parent_scope, def_ip, sub_idx)).copied()
    }

    /// Resolves `(is_stmt_entry, combined_line, source_path, orig_line)` for `(scope_id, ip)`.
    pub(crate) fn resolve(
        &self,
        scope_id: usize,
        ip: usize,
    ) -> Option<(bool, usize, PathBuf, usize)> {
        let marks = self.scopes.get(scope_id)?;
        let idx = marks.partition_point(|&(start, _)| start <= ip);
        if idx == 0 {
            return None;
        }
        let (start_ip, combined_line) = marks[idx - 1];
        let (path, orig_line) = self
            .origin_for_combined(combined_line)
            .unwrap_or_else(|| (PathBuf::from("<main>"), combined_line));
        Some((start_ip == ip, combined_line, path, orig_line))
    }

    pub(crate) fn snap_breakpoint(&self, path: &Path, requested_line: usize) -> usize {
        let key = normalize_path(path);
        if let Some(lines) = self.valid_lines.get(&key) {
            if let Some(&snapped) = lines.iter().find(|&&line| line >= requested_line) {
                return snapped;
            }
        }
        requested_line
    }
}

pub(crate) fn normalize_path(path: &Path) -> String {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let s = canonical.to_string_lossy().to_string();
    if cfg!(windows) {
        let stripped = s.strip_prefix(r"\\?\").unwrap_or(&s);
        stripped.to_lowercase()
    } else {
        s
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StepAction {
    Run,
    StepIn,
    StepOver { target_depth: usize },
    StepOut { target_depth: usize },
    PauseNext,
}

#[derive(Clone, Debug)]
pub(crate) struct ActiveFrame {
    pub(crate) id: usize,
    pub(crate) name: String,
    pub(crate) ip: usize,
    pub(crate) source: PathBuf,
    pub(crate) line: usize,
    pub(crate) vars: HashMap<String, Val>,
}

const MAX_TRACE_SNAPSHOTS: usize = 4096;

#[derive(Clone, Debug)]
struct TraceSnapshot {
    frames: Vec<ActiveFrame>,
    #[allow(dead_code)]
    last_stop_site: Option<(usize, usize)>,
}

struct DebugState {
    configured: bool,
    terminated: bool,
    paused: bool,
    stop_on_entry: bool,
    step: StepAction,
    breakpoints: HashMap<String, HashSet<usize>>,
    data_breakpoints: HashSet<String>,
    last_watched_values: HashMap<String, Option<String>>,
    frames: Vec<ActiveFrame>,
    history: VecDeque<TraceSnapshot>,
    history_cursor: Option<usize>,
    next_frame_id: usize,
    last_stop_site: Option<(usize, usize)>,
    funcs: Arc<HashMap<String, FunctionDef>>,
    classes: Arc<HashMap<String, ClassDef>>,
    objects: Arc<Mutex<HashMap<u64, ObjectData>>>,
}

#[derive(Clone)]
pub(crate) struct DapSink {
    seq: Arc<AtomicU64>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
}

impl DapSink {
    pub(crate) fn new(writer: Box<dyn Write + Send>) -> Self {
        Self {
            seq: Arc::new(AtomicU64::new(1)),
            writer: Arc::new(Mutex::new(writer)),
        }
    }

    fn send(&self, mut payload: JsonValue) -> Result<(), String> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("seq".into(), json!(seq));
        }
        let body = serde_json::to_string(&payload).map_err(|e| e.to_string())?;
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        let mut w = self.writer.lock().map_err(|_| "dap writer poisoned")?;
        w.write_all(header.as_bytes()).map_err(|e| e.to_string())?;
        w.write_all(body.as_bytes()).map_err(|e| e.to_string())?;
        w.flush().map_err(|e| e.to_string())
    }

    pub(crate) fn send_response(
        &self,
        request_seq: u64,
        command: &str,
        success: bool,
        body: Option<JsonValue>,
        message: Option<String>,
    ) -> Result<(), String> {
        let mut msg = json!({
            "type": "response",
            "request_seq": request_seq,
            "success": success,
            "command": command,
        });
        if let Some(b) = body {
            msg["body"] = b;
        }
        if let Some(m) = message {
            msg["message"] = json!(m);
        }
        self.send(msg)
    }

    pub(crate) fn send_event(&self, event: &str, body: Option<JsonValue>) -> Result<(), String> {
        let mut msg = json!({
            "type": "event",
            "event": event,
        });
        if let Some(b) = body {
            msg["body"] = b;
        }
        self.send(msg)
    }
}

pub(crate) struct DebugSession {
    pub(crate) map: DebugMap,
    sink: DapSink,
    program_path: PathBuf,
    state: Mutex<DebugState>,
    wake: Condvar,
    var_refs: Mutex<HashMap<usize, Vec<(String, Val)>>>,
    next_var_ref: AtomicU64,
}

impl DebugSession {
    pub(crate) fn new(
        map: DebugMap,
        program_path: PathBuf,
        stop_on_entry: bool,
        sink: DapSink,
    ) -> Self {
        Self {
            map,
            sink,
            program_path,
            state: Mutex::new(DebugState {
                configured: false,
                terminated: false,
                paused: false,
                stop_on_entry,
                step: StepAction::Run,
                breakpoints: HashMap::new(),
                data_breakpoints: HashSet::new(),
                last_watched_values: HashMap::new(),
                frames: Vec::new(),
                history: VecDeque::new(),
                history_cursor: None,
                next_frame_id: 1,
                last_stop_site: None,
                funcs: Arc::new(HashMap::new()),
                classes: Arc::new(HashMap::new()),
                objects: Arc::new(Mutex::new(HashMap::new())),
            }),
            wake: Condvar::new(),
            var_refs: Mutex::new(HashMap::new()),
            next_var_ref: AtomicU64::new(100),
        }
    }

    pub(crate) fn child_scope(
        &self,
        parent_scope: usize,
        def_ip: usize,
        sub_idx: usize,
    ) -> Option<usize> {
        self.map.child_scope(parent_scope, def_ip, sub_idx)
    }

    pub(crate) fn emit_output(&self, text: &str) {
        let _ = self.sink.send_event(
            "output",
            Some(json!({
                "category": "stdout",
                "output": format!("{text}\n")
            })),
        );
    }

    pub(crate) fn frame_depth(&self) -> usize {
        self.state.lock().map(|g| g.frames.len()).unwrap_or(0)
    }

    pub(crate) fn push_frame(&self, name: String, scope_id: usize) {
        if let Ok(mut g) = self.state.lock() {
            let id = g.next_frame_id;
            g.next_frame_id += 1;
            let initial_line = self
                .map
                .resolve(scope_id, 0)
                .map(|(_, _, path, line)| (path, line))
                .unwrap_or_else(|| (self.program_path.clone(), 1));
            g.frames.push(ActiveFrame {
                id,
                name,
                ip: 0,
                source: initial_line.0,
                line: initial_line.1,
                vars: HashMap::new(),
            });
        }
    }

    pub(crate) fn pop_frame(&self) {
        if let Ok(mut g) = self.state.lock() {
            g.frames.pop();
        }
    }

    pub(crate) fn update_caller_frame(
        &self,
        scope_id: usize,
        ip: usize,
        vars: &HashMap<String, Val>,
        cells: &HashMap<String, Arc<Mutex<Val>>>,
        locals: &[Val],
        local_defined: &[bool],
    ) {
        let merged = self
            .map
            .merge_frame_vars(scope_id, vars, cells, locals, local_defined);
        let loc = self.map.resolve(scope_id, ip);
        if let Ok(mut g) = self.state.lock() {
            if let Some(frame) = g.frames.last_mut() {
                frame.ip = ip;
                frame.vars = merged;
                if let Some((_, _, path, line)) = loc {
                    frame.source = path;
                    frame.line = line;
                }
            }
        }
    }

    pub(crate) fn set_breakpoints(
        &self,
        path: &Path,
        requested: &[usize],
    ) -> Result<Vec<JsonValue>, String> {
        let mut snapped_set = HashSet::new();
        let mut results = Vec::with_capacity(requested.len());
        for &req in requested {
            let line = self.map.snap_breakpoint(path, req);
            snapped_set.insert(line);
            results.push(json!({
                "verified": true,
                "line": line
            }));
        }
        let key = normalize_path(path);
        let mut g = self.state.lock().map_err(|_| "dap state poisoned")?;
        if snapped_set.is_empty() {
            g.breakpoints.remove(&key);
        } else {
            g.breakpoints.insert(key, snapped_set);
        }
        Ok(results)
    }

    pub(crate) fn set_data_breakpoints(&self, names: &[String]) -> Result<Vec<JsonValue>, String> {
        let mut g = self.state.lock().map_err(|_| "dap state poisoned")?;
        g.data_breakpoints.clear();
        g.last_watched_values.clear();
        let current_frames: Vec<ActiveFrame> = Self::displayed_frames(&g).to_vec();
        let mut results = Vec::with_capacity(names.len());
        for (idx, raw) in names.iter().enumerate() {
            let clean = raw.trim();
            if clean.is_empty() {
                results.push(json!({ "verified": false, "id": idx + 1 }));
                continue;
            }
            g.data_breakpoints.insert(clean.to_string());
            let baseline = current_frames
                .iter()
                .rev()
                .find_map(|f| f.vars.get(clean).map(Val::show));
            g.last_watched_values.insert(clean.to_string(), baseline);
            results.push(json!({ "verified": true, "id": idx + 1 }));
        }
        Ok(results)
    }

    fn snapshot_var_repr(frames: &[ActiveFrame], name: &str) -> Option<String> {
        frames
            .iter()
            .rev()
            .find_map(|f| f.vars.get(name).map(Val::show))
    }

    pub(crate) fn mark_configured(&self) -> Result<(), String> {
        let mut g = self.state.lock().map_err(|_| "dap state poisoned")?;
        g.configured = true;
        self.wake.notify_all();
        Ok(())
    }

    fn displayed_frames(g: &DebugState) -> &[ActiveFrame] {
        if let Some(idx) = g.history_cursor
            && let Some(snap) = g.history.get(idx)
        {
            &snap.frames
        } else {
            &g.frames
        }
    }

    pub(crate) fn resume(&self, action: StepAction) -> Result<(), String> {
        let mut g = self.state.lock().map_err(|_| "dap state poisoned")?;
        if let Some(idx) = g.history_cursor {
            let at_history_step = matches!(
                action,
                StepAction::StepIn | StepAction::StepOver { .. } | StepAction::StepOut { .. }
            );
            if at_history_step && idx + 1 < g.history.len() {
                g.history_cursor = Some(idx + 1);
                if let Ok(mut refs) = self.var_refs.lock() {
                    refs.clear();
                }
                drop(g);
                let _ = self.sink.send_event(
                    "stopped",
                    Some(json!({
                        "reason": "step",
                        "threadId": 1,
                        "allThreadsStopped": true
                    })),
                );
                return Ok(());
            }
            if at_history_step && idx + 1 == g.history.len() {
                // Stepping from the last historic frame restores the live stop frame first.
                g.history_cursor = None;
                if let Ok(mut refs) = self.var_refs.lock() {
                    refs.clear();
                }
                drop(g);
                let _ = self.sink.send_event(
                    "stopped",
                    Some(json!({
                        "reason": "step",
                        "threadId": 1,
                        "allThreadsStopped": true
                    })),
                );
                return Ok(());
            }
            g.history_cursor = None;
        }
        let depth = g.frames.len();
        g.step = match action {
            StepAction::StepOver { .. } => StepAction::StepOver {
                target_depth: depth,
            },
            StepAction::StepOut { .. } => StepAction::StepOut {
                target_depth: depth.saturating_sub(1),
            },
            other => other,
        };
        g.paused = false;
        self.wake.notify_all();
        Ok(())
    }

    pub(crate) fn step_back(&self) -> Result<(), String> {
        let mut g = self.state.lock().map_err(|_| "dap state poisoned")?;
        if g.history.is_empty() {
            return Err("no recorded trace history to step back into".into());
        }
        let cur = g
            .history_cursor
            .unwrap_or_else(|| g.history.len().saturating_sub(1));
        let prev = cur.saturating_sub(1);
        g.history_cursor = Some(prev);
        if let Ok(mut refs) = self.var_refs.lock() {
            refs.clear();
        }
        drop(g);
        let _ = self.sink.send_event(
            "stopped",
            Some(json!({
                "reason": "step",
                "threadId": 1,
                "allThreadsStopped": true
            })),
        );
        Ok(())
    }

    pub(crate) fn reverse_continue(&self) -> Result<(), String> {
        let mut g = self.state.lock().map_err(|_| "dap state poisoned")?;
        if g.history.is_empty() {
            return Err("no recorded trace history".into());
        }
        let cur = g
            .history_cursor
            .unwrap_or_else(|| g.history.len().saturating_sub(1));
        let mut target = None;
        let watched: Vec<String> = g.data_breakpoints.iter().cloned().collect();
        for idx in (0..cur).rev() {
            if let Some(snap) = g.history.get(idx)
                && let Some(top) = snap.frames.last()
            {
                let norm = normalize_path(&top.source);
                if g.breakpoints
                    .get(&norm)
                    .is_some_and(|set| set.contains(&top.line))
                {
                    target = Some((idx, "breakpoint"));
                    break;
                }
                if !watched.is_empty()
                    && let Some(next_snap) = g.history.get(idx + 1)
                {
                    let changed = watched.iter().any(|w| {
                        Self::snapshot_var_repr(&snap.frames, w)
                            != Self::snapshot_var_repr(&next_snap.frames, w)
                    });
                    if changed {
                        target = Some((idx, "data breakpoint"));
                        break;
                    }
                }
            }
        }
        let (idx, reason) = target.unwrap_or((0, "entry"));
        g.history_cursor = Some(idx);
        if let Ok(mut refs) = self.var_refs.lock() {
            refs.clear();
        }
        drop(g);
        let _ = self.sink.send_event(
            "stopped",
            Some(json!({
                "reason": reason,
                "threadId": 1,
                "allThreadsStopped": true
            })),
        );
        Ok(())
    }

    pub(crate) fn terminate(&self) {
        if let Ok(mut g) = self.state.lock() {
            g.terminated = true;
            g.paused = false;
            self.wake.notify_all();
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn before_op(
        &self,
        scope_id: usize,
        ip: usize,
        vars: &HashMap<String, Val>,
        cells: &HashMap<String, Arc<Mutex<Val>>>,
        locals: &[Val],
        local_defined: &[bool],
        funcs: &Arc<HashMap<String, FunctionDef>>,
        classes: &Arc<HashMap<String, ClassDef>>,
        objects: &Arc<Mutex<HashMap<u64, ObjectData>>>,
    ) -> Result<(), String> {
        let mut g = self.state.lock().map_err(|_| "dap state poisoned")?;
        while !g.configured && !g.terminated {
            g = self.wake.wait(g).map_err(|_| "dap state poisoned")?;
        }
        if g.terminated {
            return Err(DAP_TERMINATED_ERR.into());
        }

        let resolved = self.map.resolve(scope_id, ip);
        let (is_stmt_entry, file, line) = match resolved {
            Some((entry, _, f, l)) => (entry, f, l),
            None => (false, self.program_path.clone(), 0),
        };
        if let Some(top) = g.frames.last_mut() {
            top.ip = ip;
            if line > 0 {
                top.source = file.clone();
                top.line = line;
            }
        }
        if is_stmt_entry && line > 0 && g.last_stop_site != Some((scope_id, ip)) {
            let snap_vars = self
                .map
                .merge_frame_vars(scope_id, vars, cells, locals, local_defined);
            if let Some(top) = g.frames.last_mut() {
                top.vars = snap_vars;
            }
            if g.history.len() >= MAX_TRACE_SNAPSHOTS {
                g.history.pop_front();
            }
            let frames_snapshot = g.frames.clone();
            g.history.push_back(TraceSnapshot {
                frames: frames_snapshot,
                last_stop_site: Some((scope_id, ip)),
            });
            g.history_cursor = None;
        }

        if let Some(prev_site) = g.last_stop_site {
            if prev_site != (scope_id, ip) {
                g.last_stop_site = None;
            }
        }

        let depth = g.frames.len();
        let norm_file = normalize_path(&file);
        let mut watch_hit = false;
        if is_stmt_entry
            && line > 0
            && g.last_stop_site != Some((scope_id, ip))
            && !g.data_breakpoints.is_empty()
        {
            let watched: Vec<String> = g.data_breakpoints.iter().cloned().collect();
            for w in watched {
                let cur_repr = Self::snapshot_var_repr(&g.frames, &w);
                let prev = g.last_watched_values.get(&w).cloned().unwrap_or(None);
                if cur_repr != prev {
                    if cur_repr.is_some() || prev.is_some() {
                        watch_hit = true;
                    }
                    g.last_watched_values.insert(w, cur_repr);
                }
            }
        }
        let stop_reason = if g.stop_on_entry && is_stmt_entry && line > 0 {
            g.stop_on_entry = false;
            Some("entry")
        } else if is_stmt_entry && line > 0 && g.last_stop_site != Some((scope_id, ip)) {
            if g.breakpoints
                .get(&norm_file)
                .is_some_and(|set| set.contains(&line))
            {
                Some("breakpoint")
            } else if watch_hit {
                Some("data breakpoint")
            } else {
                match g.step {
                    StepAction::PauseNext => Some("pause"),
                    StepAction::StepIn => Some("step"),
                    StepAction::StepOver { target_depth } if depth <= target_depth => Some("step"),
                    StepAction::StepOut { target_depth } if depth <= target_depth => Some("step"),
                    _ => None,
                }
            }
        } else {
            None
        };

        let Some(reason) = stop_reason else {
            return Ok(());
        };

        let merged = self
            .map
            .merge_frame_vars(scope_id, vars, cells, locals, local_defined);
        if let Some(top) = g.frames.last_mut() {
            top.vars = merged;
        }
        g.funcs = funcs.clone();
        g.classes = classes.clone();
        g.objects = objects.clone();
        g.last_stop_site = Some((scope_id, ip));
        g.paused = true;
        g.step = StepAction::Run;

        if let Ok(mut refs) = self.var_refs.lock() {
            refs.clear();
        }
        drop(g);

        let _ = self.sink.send_event(
            "stopped",
            Some(json!({
                "reason": reason,
                "threadId": 1,
                "allThreadsStopped": true
            })),
        );

        let mut g = self.state.lock().map_err(|_| "dap state poisoned")?;
        while g.paused && !g.terminated {
            g = self.wake.wait(g).map_err(|_| "dap state poisoned")?;
        }
        if g.terminated {
            return Err(DAP_TERMINATED_ERR.into());
        }
        Ok(())
    }

    pub(crate) fn stack_frames(&self) -> Result<Vec<JsonValue>, String> {
        let g = self.state.lock().map_err(|_| "dap state poisoned")?;
        let frames = Self::displayed_frames(&g);
        let mut out = Vec::with_capacity(frames.len());
        for f in frames.iter().rev() {
            out.push(json!({
                "id": f.id,
                "name": f.name,
                "source": {
                    "name": f.source.file_name().and_then(|s| s.to_str()).unwrap_or("main.vy"),
                    "path": f.source.to_string_lossy()
                },
                "line": f.line.max(1),
                "column": 1
            }));
        }
        Ok(out)
    }

    fn alloc_var_ref(&self, entries: Vec<(String, Val)>) -> Result<usize, String> {
        let id = self.next_var_ref.fetch_add(1, Ordering::Relaxed) as usize;
        self.var_refs
            .lock()
            .map_err(|_| "var_refs poisoned")?
            .insert(id, entries);
        Ok(id)
    }

    pub(crate) fn scopes(&self, frame_id: usize) -> Result<Vec<JsonValue>, String> {
        let g = self.state.lock().map_err(|_| "dap state poisoned")?;
        let frames = Self::displayed_frames(&g);
        let frame = frames
            .iter()
            .find(|f| f.id == frame_id)
            .ok_or_else(|| format!("unknown stack frame {frame_id}"))?;
        let mut locals: Vec<(String, Val)> = frame
            .vars
            .iter()
            .filter(|(k, _)| !k.starts_with("__"))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        locals.sort_by(|a, b| a.0.cmp(&b.0));
        let locals_ref = self.alloc_var_ref(locals)?;

        let mut scopes = vec![json!({
            "name": "Locals",
            "variablesReference": locals_ref,
            "expensive": false
        })];

        if frames.first().is_some_and(|root| root.id != frame.id) {
            let mut globals: Vec<(String, Val)> = frames
                .first()
                .map(|root| {
                    root.vars
                        .iter()
                        .filter(|(k, _)| !k.starts_with("__"))
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect()
                })
                .unwrap_or_default();
            globals.sort_by(|a, b| a.0.cmp(&b.0));
            let globals_ref = self.alloc_var_ref(globals)?;
            scopes.push(json!({
                "name": "Globals",
                "variablesReference": globals_ref,
                "expensive": false
            }));
        }
        Ok(scopes)
    }

    pub(crate) fn variables(&self, var_ref: usize) -> Result<Vec<JsonValue>, String> {
        let entries = self
            .var_refs
            .lock()
            .map_err(|_| "var_refs poisoned")?
            .get(&var_ref)
            .cloned()
            .unwrap_or_default();
        let objects = self
            .state
            .lock()
            .map_err(|_| "dap state poisoned")?
            .objects
            .clone();
        let mut out = Vec::with_capacity(entries.len());
        for (name, val) in entries {
            let child_entries = expandable_children(&val, &objects);
            let child_ref = if child_entries.is_empty() {
                0
            } else {
                self.alloc_var_ref(child_entries)?
            };
            out.push(json!({
                "name": name,
                "value": val.show(),
                "type": val_type_name(&val, &objects),
                "variablesReference": child_ref
            }));
        }
        Ok(out)
    }

    pub(crate) fn evaluate(
        &self,
        expr: &str,
        frame_id: Option<usize>,
    ) -> Result<JsonValue, String> {
        let g = self.state.lock().map_err(|_| "dap state poisoned")?;
        let frames = Self::displayed_frames(&g);
        let frame = frame_id
            .and_then(|id| frames.iter().find(|f| f.id == id))
            .or_else(|| frames.last())
            .ok_or_else(|| "no paused stack frame".to_string())?;
        let trimmed = expr.trim();
        if let Some(v) = frame.vars.get(trimmed) {
            return Ok(json!({
                "result": v.show(),
                "type": val_type_name(v, &g.objects),
                "variablesReference": 0
            }));
        }
        let code = compile_eval_expr(trimmed)?;
        let mut vars = frames
            .first()
            .map(|root| root.vars.clone())
            .unwrap_or_default();
        for (k, v) in &frame.vars {
            vars.insert(k.clone(), v.clone());
        }
        let mut vm = Vm::new(code);
        vm.vars = vars;
        vm.funcs = g.funcs.clone();
        vm.classes = g.classes.clone();
        vm.objects = g.objects.clone();
        drop(g);

        let value = vm.execute()?;
        let objects = self
            .state
            .lock()
            .map_err(|_| "dap state poisoned")?
            .objects
            .clone();
        Ok(json!({
            "result": value.show(),
            "type": val_type_name(&value, &objects),
            "variablesReference": 0
        }))
    }
}

pub(crate) struct DebugFrameGuard {
    session: Arc<DebugSession>,
}

impl DebugFrameGuard {
    pub(crate) fn push(session: Arc<DebugSession>, name: String, scope_id: usize) -> Self {
        session.push_frame(name, scope_id);
        Self { session }
    }
}

impl Drop for DebugFrameGuard {
    fn drop(&mut self) {
        self.session.pop_frame();
    }
}

fn merge_vars(
    vars: &HashMap<String, Val>,
    cells: &HashMap<String, Arc<Mutex<Val>>>,
) -> HashMap<String, Val> {
    let mut merged = vars.clone();
    for (k, cell) in cells {
        if let Ok(guard) = cell.lock() {
            merged.insert(k.clone(), guard.clone());
        }
    }
    merged
}

fn val_type_name(val: &Val, objects: &Arc<Mutex<HashMap<u64, ObjectData>>>) -> String {
    match val {
        Val::Num(n) if n.fract() == 0.0 => "Int".into(),
        Val::Num(_) => "Number".into(),
        Val::Str(_) => "String".into(),
        Val::Bool(_) => "Bool".into(),
        Val::Null => "Null".into(),
        Val::List(_) => "List".into(),
        Val::Map(_) => "Map".into(),
        Val::Function(_) => "Function".into(),
        Val::Generator(_) => "Generator".into(),
        Val::Object(id) => objects
            .lock()
            .ok()
            .and_then(|heap| heap.get(id).map(|o| o.class_name.clone()))
            .unwrap_or_else(|| "Object".into()),
    }
}

fn expandable_children(
    val: &Val,
    objects: &Arc<Mutex<HashMap<u64, ObjectData>>>,
) -> Vec<(String, Val)> {
    match val {
        Val::List(items) => items
            .iter()
            .enumerate()
            .map(|(i, v)| (format!("[{i}]"), v.clone()))
            .collect(),
        Val::Map(pairs) => pairs.iter().map(|(k, v)| (k.show(), v.clone())).collect(),
        Val::Object(id) => {
            let mut fields: Vec<(String, Val)> = objects
                .lock()
                .ok()
                .and_then(|heap| {
                    heap.get(id).map(|obj| {
                        obj.fields
                            .iter()
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect()
                    })
                })
                .unwrap_or_default();
            fields.sort_by(|a, b| a.0.cmp(&b.0));
            fields
        }
        _ => Vec::new(),
    }
}

pub(crate) fn read_dap_message(reader: &mut impl BufRead) -> Result<Option<JsonValue>, String> {
    let mut content_len: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).map_err(|e| e.to_string())?;
        if n == 0 {
            return Ok(None);
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if content_len.is_some() {
                break;
            }
            continue;
        }
        if let Some((name, val)) = trimmed.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                content_len = Some(
                    val.trim()
                        .parse::<usize>()
                        .map_err(|e| format!("invalid Content-Length: {e}"))?,
                );
            }
        }
    }
    let len = content_len.ok_or("missing Content-Length header")?;
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).map_err(|e| e.to_string())?;
    serde_json::from_slice(&buf)
        .map(Some)
        .map_err(|e| format!("invalid DAP JSON: {e}"))
}

struct DapServer {
    sink: DapSink,
    session: Option<Arc<DebugSession>>,
    vm_thread: Option<thread::JoinHandle<()>>,
    should_exit: Arc<AtomicBool>,
}

impl DapServer {
    fn new(sink: DapSink) -> Self {
        Self {
            sink,
            session: None,
            vm_thread: None,
            should_exit: Arc::new(AtomicBool::new(false)),
        }
    }

    fn handle_message(&mut self, msg: JsonValue) -> Result<bool, String> {
        let seq = msg.get("seq").and_then(JsonValue::as_u64).unwrap_or(0);
        let cmd = msg.get("command").and_then(JsonValue::as_str).unwrap_or("");
        let args = msg.get("arguments").cloned().unwrap_or_else(|| json!({}));

        match cmd {
            "initialize" => {
                self.sink.send_response(
                    seq,
                    cmd,
                    true,
                    Some(json!({
                        "supportsConfigurationDoneRequest": true,
                        "supportsEvaluateForHovers": true,
                        "supportsStepBack": true,
                        "supportsDataBreakpoints": true,
                        "supportsTerminateRequest": true,
                        "supportsSingleThreadExecutionRequests": true
                    })),
                    None,
                )?;
                self.sink.send_event("initialized", None)?;
            }
            "launch" => {
                let program = args
                    .get("program")
                    .and_then(JsonValue::as_str)
                    .ok_or("launch missing 'program' path")?;
                let stop_on_entry = args
                    .get("stopOnEntry")
                    .and_then(JsonValue::as_bool)
                    .unwrap_or(false);
                let program_path = PathBuf::from(program);
                let (code, map) = match compile_file_with_debug(program) {
                    Ok(compiled) => compiled,
                    Err(e) => {
                        self.sink.send_response(seq, cmd, false, None, Some(e))?;
                        return Ok(false);
                    }
                };
                let session = Arc::new(DebugSession::new(
                    map,
                    program_path,
                    stop_on_entry,
                    self.sink.clone(),
                ));
                let session_for_vm = session.clone();
                let sink_for_vm = self.sink.clone();
                let handle = thread::spawn(move || {
                    let mut vm = Vm::new(code);
                    vm.debugger = Some(session_for_vm.clone());
                    let res = vm.run();
                    let exit_code = match res {
                        Ok(()) => 0,
                        Err(e) if e == DAP_TERMINATED_ERR => 0,
                        Err(e) => {
                            let _ = sink_for_vm.send_event(
                                "output",
                                Some(json!({
                                    "category": "stderr",
                                    "output": format!("Runtime error: {e}\n")
                                })),
                            );
                            1
                        }
                    };
                    let _ =
                        sink_for_vm.send_event("exited", Some(json!({ "exitCode": exit_code })));
                    let _ = sink_for_vm.send_event("terminated", None);
                });
                self.session = Some(session);
                self.vm_thread = Some(handle);
                self.sink.send_response(seq, cmd, true, None, None)?;
            }
            "setBreakpoints" => {
                let path_str = args
                    .pointer("/source/path")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("");
                let lines: Vec<usize> = args
                    .get("breakpoints")
                    .and_then(JsonValue::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|b| b.get("line").and_then(JsonValue::as_u64))
                            .map(|n| n as usize)
                            .collect()
                    })
                    .unwrap_or_default();
                let bps = if let Some(ref s) = self.session {
                    s.set_breakpoints(Path::new(path_str), &lines)?
                } else {
                    lines
                        .into_iter()
                        .map(|line| json!({"verified": false, "line": line}))
                        .collect()
                };
                self.sink.send_response(
                    seq,
                    cmd,
                    true,
                    Some(json!({ "breakpoints": bps })),
                    None,
                )?;
            }
            "dataBreakpointInfo" => {
                let name = args
                    .get("name")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("")
                    .trim();
                let data_id = if name.is_empty() {
                    JsonValue::Null
                } else {
                    JsonValue::String(name.to_string())
                };
                self.sink.send_response(
                    seq,
                    cmd,
                    true,
                    Some(json!({
                        "dataId": data_id,
                        "description": if name.is_empty() {
                            "No watchable variable specified".to_string()
                        } else {
                            format!("Break on value change of '{name}'")
                        },
                        "accessTypes": ["write"],
                        "canPersist": false
                    })),
                    None,
                )?;
            }
            "setDataBreakpoints" => {
                let names: Vec<String> = args
                    .get("breakpoints")
                    .and_then(JsonValue::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|b| b.get("dataId").and_then(JsonValue::as_str))
                            .map(|s| s.to_string())
                            .collect()
                    })
                    .unwrap_or_default();
                let bps = if let Some(ref s) = self.session {
                    s.set_data_breakpoints(&names)?
                } else {
                    names
                        .iter()
                        .enumerate()
                        .map(|(i, _)| json!({"verified": false, "id": i + 1}))
                        .collect()
                };
                self.sink.send_response(
                    seq,
                    cmd,
                    true,
                    Some(json!({ "breakpoints": bps })),
                    None,
                )?;
            }
            "configurationDone" => {
                if let Some(ref s) = self.session {
                    s.mark_configured()?;
                }
                self.sink.send_response(seq, cmd, true, None, None)?;
            }
            "threads" => {
                self.sink.send_response(
                    seq,
                    cmd,
                    true,
                    Some(json!({
                        "threads": [{ "id": 1, "name": "main" }]
                    })),
                    None,
                )?;
            }
            "stackTrace" => {
                let frames = self
                    .session
                    .as_ref()
                    .map(|s| s.stack_frames())
                    .transpose()?
                    .unwrap_or_default();
                let total = frames.len();
                self.sink.send_response(
                    seq,
                    cmd,
                    true,
                    Some(json!({ "stackFrames": frames, "totalFrames": total })),
                    None,
                )?;
            }
            "scopes" => {
                let frame_id =
                    args.get("frameId").and_then(JsonValue::as_u64).unwrap_or(1) as usize;
                let scopes = self
                    .session
                    .as_ref()
                    .map(|s| s.scopes(frame_id))
                    .transpose()?
                    .unwrap_or_default();
                self.sink
                    .send_response(seq, cmd, true, Some(json!({ "scopes": scopes })), None)?;
            }
            "variables" => {
                let var_ref = args
                    .get("variablesReference")
                    .and_then(JsonValue::as_u64)
                    .unwrap_or(0) as usize;
                let vars = self
                    .session
                    .as_ref()
                    .map(|s| s.variables(var_ref))
                    .transpose()?
                    .unwrap_or_default();
                self.sink.send_response(
                    seq,
                    cmd,
                    true,
                    Some(json!({ "variables": vars })),
                    None,
                )?;
            }
            "continue" => {
                if let Some(ref s) = self.session {
                    s.resume(StepAction::Run)?;
                }
                self.sink.send_response(
                    seq,
                    cmd,
                    true,
                    Some(json!({ "allThreadsContinued": true })),
                    None,
                )?;
            }
            "next" => {
                if let Some(ref s) = self.session {
                    s.resume(StepAction::StepOver { target_depth: 0 })?;
                }
                self.sink.send_response(seq, cmd, true, None, None)?;
            }
            "stepIn" => {
                if let Some(ref s) = self.session {
                    s.resume(StepAction::StepIn)?;
                }
                self.sink.send_response(seq, cmd, true, None, None)?;
            }
            "stepOut" => {
                if let Some(ref s) = self.session {
                    s.resume(StepAction::StepOut { target_depth: 0 })?;
                }
                self.sink.send_response(seq, cmd, true, None, None)?;
            }
            "pause" => {
                if let Some(ref s) = self.session {
                    s.resume(StepAction::PauseNext)?;
                }
                self.sink.send_response(seq, cmd, true, None, None)?;
            }
            "stepBack" => {
                if let Some(ref s) = self.session {
                    s.step_back()?;
                }
                self.sink.send_response(seq, cmd, true, None, None)?;
            }
            "reverseContinue" => {
                if let Some(ref s) = self.session {
                    s.reverse_continue()?;
                }
                self.sink.send_response(seq, cmd, true, None, None)?;
            }
            "evaluate" => {
                let expr = args
                    .get("expression")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("");
                let frame_id = args
                    .get("frameId")
                    .and_then(JsonValue::as_u64)
                    .map(|n| n as usize);
                match self.session.as_ref() {
                    Some(s) => match s.evaluate(expr, frame_id) {
                        Ok(body) => self.sink.send_response(seq, cmd, true, Some(body), None)?,
                        Err(e) => self.sink.send_response(seq, cmd, false, None, Some(e))?,
                    },
                    None => self.sink.send_response(
                        seq,
                        cmd,
                        false,
                        None,
                        Some("no active debug session".into()),
                    )?,
                }
            }
            "disconnect" | "terminate" => {
                if let Some(ref s) = self.session {
                    s.terminate();
                }
                self.should_exit.store(true, Ordering::Relaxed);
                self.sink.send_response(seq, cmd, true, None, None)?;
                return Ok(true);
            }
            _ => {
                self.sink.send_response(seq, cmd, true, None, None)?;
            }
        }
        Ok(false)
    }

    fn shutdown(mut self) {
        if let Some(ref s) = self.session {
            s.terminate();
        }
        if let Some(handle) = self.vm_thread.take() {
            let _ = handle.join();
        }
    }
}

pub(crate) fn run_dap_with_io(
    mut reader: impl BufRead,
    writer: Box<dyn Write + Send>,
) -> Result<(), String> {
    let sink = DapSink::new(writer);
    let mut server = DapServer::new(sink);
    while let Some(msg) = read_dap_message(&mut reader)? {
        if msg.get("type").and_then(JsonValue::as_str) == Some("request") {
            match server.handle_message(msg) {
                Ok(true) => break,
                Ok(false) => {}
                Err(e) => return Err(e),
            }
        }
    }
    server.shutdown();
    Ok(())
}

pub(crate) fn dap_server() -> Result<(), String> {
    let stdin = io::stdin();
    let reader = io::BufReader::new(stdin.lock());
    run_dap_with_io(reader, Box::new(io::stdout()))
}
