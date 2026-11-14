// Universal OS, Filesystem, Process Supervisor, Node.js/npm Bridge, Concurrent
// Web/SSE App Server, and Electron-Class Desktop WebView Shell Architecture.

use std::collections::{HashMap, VecDeque};
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, UNIX_EPOCH};

use base64::Engine;
use serde_json::{Value as JsonValue, json};
use tiny_http::{Header, Response, Server, StatusCode};

use crate::pipeline::verify_source_strict;
use crate::registry::safe_package_relative_path;
use crate::runtime::{channels, require_cap};
use crate::value::{Val, json_to_val, map_lookup, val_to_json};
use crate::vm::Vm;

static PROC_IDS: AtomicU64 = AtomicU64::new(1);
static PROCS: OnceLock<Mutex<HashMap<u64, ManagedProc>>> = OnceLock::new();
static NODE_BRIDGE: OnceLock<Mutex<Option<NodeWorker>>> = OnceLock::new();

fn procs() -> &'static Mutex<HashMap<u64, ManagedProc>> {
    PROCS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn node_bridge_slot() -> &'static Mutex<Option<NodeWorker>> {
    NODE_BRIDGE.get_or_init(|| Mutex::new(None))
}

struct ManagedProc {
    child: Child,
    stdin: Option<ChildStdin>,
    out_buf: Arc<Mutex<VecDeque<String>>>,
    err_buf: Arc<Mutex<VecDeque<String>>>,
    running: Arc<AtomicBool>,
    exit_code: Arc<Mutex<Option<i32>>>,
}

struct NodeWorker {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
    next_id: u64,
}

fn str_arg(val: &Val, name: &str) -> Result<String, String> {
    match val {
        Val::Str(s) => Ok(s.clone()),
        _ => Err(format!("{name} must be String")),
    }
}

fn push_ring_line(buf: &Mutex<VecDeque<String>>, line: String) {
    if let Ok(mut q) = buf.lock() {
        if q.len() >= 4000 {
            q.pop_front();
        }
        q.push_back(line);
    }
}

fn drain_ring_lines(buf: &Mutex<VecDeque<String>>) -> Vec<String> {
    buf.lock()
        .map(|mut q| q.drain(..).collect())
        .unwrap_or_default()
}

fn modified_epoch_ms(meta: &fs::Metadata) -> f64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

fn apply_map_str_list(pairs: &[(Val, Val)], key: &str) -> Option<Vec<String>> {
    let Val::List(items) = map_lookup(pairs, &Val::Str(key.into()))? else {
        return None;
    };
    Some(items.iter().map(Val::show).collect())
}

fn apply_map_usize(pairs: &[(Val, Val)], key: &str, default: usize) -> usize {
    match map_lookup(pairs, &Val::Str(key.into())) {
        Some(Val::Num(n)) if *n >= 1.0 => *n as usize,
        _ => default,
    }
}

fn apply_map_u64(pairs: &[(Val, Val)], key: &str, default: u64) -> u64 {
    match map_lookup(pairs, &Val::Str(key.into())) {
        Some(Val::Num(n)) if *n >= 1.0 => *n as u64,
        _ => default,
    }
}

struct WalkConfig<'a> {
    root: &'a Path,
    max_depth: Option<usize>,
    skip_dirs: &'a [String],
    exts: Option<&'a [String]>,
    max_files: usize,
    max_bytes: u64,
}

fn walk_dir_rec(
    cfg: &WalkConfig<'_>,
    dir: &Path,
    depth: usize,
    out: &mut Vec<Val>,
) -> Result<(), String> {
    if out.len() >= cfg.max_files || cfg.max_depth.is_some_and(|md| depth > md) {
        return Ok(());
    }
    let mut entries: Vec<fs::DirEntry> = fs::read_dir(dir)
        .map_err(|e| e.to_string())?
        .filter_map(Result::ok)
        .collect();
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        if out.len() >= cfg.max_files {
            break;
        }
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            if cfg.skip_dirs.iter().any(|s| s.eq_ignore_ascii_case(&name)) {
                continue;
            }
            if cfg.max_depth.is_none_or(|md| depth < md) {
                walk_dir_rec(cfg, &path, depth + 1, out)?;
            }
        } else if ft.is_file() {
            let Ok(meta) = entry.metadata() else { continue };
            if meta.len() > cfg.max_bytes {
                continue;
            }
            if let Some(allow) = cfg.exts {
                let ext = path
                    .extension()
                    .and_then(|x| x.to_str())
                    .map(|x| format!(".{}", x.to_ascii_lowercase()))
                    .unwrap_or_default();
                if !allow.iter().any(|a| {
                    let norm = if a.starts_with('.') {
                        a.to_ascii_lowercase()
                    } else {
                        format!(".{}", a.to_ascii_lowercase())
                    };
                    norm == ext
                }) {
                    continue;
                }
            }
            let rel = path
                .strip_prefix(cfg.root)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string();
            out.push(Val::Map(vec![
                (Val::Str("name".into()), Val::Str(name)),
                (
                    Val::Str("path".into()),
                    Val::Str(path.to_string_lossy().to_string()),
                ),
                (Val::Str("rel_path".into()), Val::Str(rel)),
                (Val::Str("size".into()), Val::Num(meta.len() as f64)),
            ]));
        }
    }
    Ok(())
}

// ── Node.js & npm Runtime Discovery + Persistent Worker ────────────────────

pub(crate) fn find_node_binary() -> Option<PathBuf> {
    if let Ok(custom) = env::var("KOVREK_NODE_BIN") {
        let p = PathBuf::from(custom);
        if p.is_file() {
            return Some(p);
        }
    }
    let candidates: Vec<&str> = if cfg!(windows) {
        vec![
            "node.exe",
            "node",
            r"C:\nvm4w\nodejs\node.exe",
            r"C:\Program Files\nodejs\node.exe",
            r"C:\Program Files (x86)\nodejs\node.exe",
        ]
    } else {
        vec!["node", "/usr/local/bin/node", "/usr/bin/node"]
    };
    for cand in candidates {
        let p = PathBuf::from(cand);
        if p.is_absolute() {
            if p.is_file() {
                return Some(p);
            }
        } else if let Ok(out) = Command::new(cand).arg("--version").output()
            && out.status.success()
        {
            return Some(p);
        }
    }
    None
}

pub(crate) fn find_npm_binary() -> Option<String> {
    let candidates: Vec<&str> = if cfg!(windows) {
        vec![
            "npm.cmd",
            "npm",
            r"C:\nvm4w\nodejs\npm.cmd",
            r"C:\Program Files\nodejs\npm.cmd",
        ]
    } else {
        vec!["npm", "/usr/local/bin/npm", "/usr/bin/npm"]
    };
    for cand in candidates {
        let p = Path::new(cand);
        if p.is_absolute() && !p.is_file() {
            continue;
        }
        if let Ok(out) = Command::new(cand).arg("--version").output()
            && out.status.success()
        {
            return Some(cand.to_string());
        }
    }
    None
}

const NODE_WORKER_SCRIPT: &str = r#"
const readline = require('readline');
const path = require('path');
const { createRequire } = require('module');
const { pathToFileURL } = require('url');
const loaded = new Map();

async function loadMod(name) {
  if (loaded.has(name)) return loaded.get(name);
  const req = createRequire(path.join(process.cwd(), '__kovrek_anchor__.js'));
  try {
    const m = req(name);
    loaded.set(name, m);
    return m;
  } catch (e) {
    const target = path.isAbsolute(name) || name.startsWith('.') ? pathToFileURL(path.resolve(name)).href : name;
    const m = await import(target);
    loaded.set(name, m);
    return m;
  }
}

const rl = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });
rl.on('line', async (line) => {
  if (!line || !line.trim()) return;
  let msg;
  try { msg = JSON.parse(line); } catch (e) {
    process.stdout.write(JSON.stringify({ id: 0, ok: false, error: String(e) }) + '\n');
    return;
  }
  const id = msg.id || 0;
  try {
    if (msg.op === 'eval') {
      let fn;
      try {
        fn = new Function('require', 'process', 'console', `return (async () => (${msg.code}))();`);
      } catch (_syntaxErr) {
        fn = new Function('require', 'process', 'console', `return (async () => { ${msg.code} })();`);
      }
      const req = createRequire(path.join(process.cwd(), '__kovrek_anchor__.js'));
      const val = await fn(req, process, console);
      process.stdout.write(JSON.stringify({ id, ok: true, value: val === undefined ? null : val }) + '\n');
    } else if (msg.op === 'require') {
      const m = await loadMod(msg.module);
      const keys = m && typeof m === 'object' ? Object.keys(m).slice(0, 64) : [];
      process.stdout.write(JSON.stringify({ id, ok: true, value: { module: msg.module, type: typeof m, exports: keys } }) + '\n');
    } else if (msg.op === 'call') {
      const m = msg.module ? await loadMod(msg.module) : globalThis;
      const target = msg.fn ? m[msg.fn] : m;
      if (typeof target !== 'function') {
        throw new Error(`JS symbol '${msg.fn}' is not callable in '${msg.module}'`);
      }
      const val = await Promise.resolve(target(...(msg.args || [])));
      process.stdout.write(JSON.stringify({ id, ok: true, value: val === undefined ? null : val }) + '\n');
    } else {
      throw new Error(`Unknown Node bridge op '${msg.op}'`);
    }
  } catch (err) {
    process.stdout.write(JSON.stringify({ id, ok: false, error: err && err.stack ? String(err.message || err) : String(err) }) + '\n');
  }
});
"#;

fn spawn_node_worker() -> Result<NodeWorker, String> {
    let bin = find_node_binary().ok_or("Node.js runtime not found in PATH")?;
    let mut child = Command::new(bin)
        .arg("-e")
        .arg(NODE_WORKER_SCRIPT)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("failed to launch Node.js worker: {e}"))?;
    let stdin = child.stdin.take().ok_or("failed to open Node stdin")?;
    let stdout = BufReader::new(child.stdout.take().ok_or("failed to open Node stdout")?);
    Ok(NodeWorker {
        child,
        stdin,
        stdout,
        next_id: 1,
    })
}

fn node_rpc(payload: JsonValue) -> Result<Val, String> {
    let mut guard = node_bridge_slot()
        .lock()
        .map_err(|_| "Node bridge mutex poisoned")?;
    let need_respawn = match guard.as_mut() {
        Some(w) => !matches!(w.child.try_wait(), Ok(None)),
        None => true,
    };
    if need_respawn {
        *guard = Some(spawn_node_worker()?);
    }
    let worker = guard.as_mut().ok_or("Node worker unavailable")?;
    let id = worker.next_id;
    worker.next_id += 1;

    let mut req = payload;
    if let Some(obj) = req.as_object_mut() {
        obj.insert("id".into(), JsonValue::from(id));
    }
    let line = serde_json::to_string(&req).map_err(|e| e.to_string())?;
    if writeln!(worker.stdin, "{line}").is_err() || worker.stdin.flush().is_err() {
        // Retry once cleanly with fresh worker if broken pipe occurred.
        *worker = spawn_node_worker()?;
        writeln!(worker.stdin, "{line}").map_err(|e| e.to_string())?;
        worker.stdin.flush().map_err(|e| e.to_string())?;
    }

    let mut resp_line = String::new();
    worker
        .stdout
        .read_line(&mut resp_line)
        .map_err(|e| format!("Node bridge read failed: {e}"))?;
    if resp_line.trim().is_empty() {
        *guard = None;
        return Err("Node worker closed unexpectedly".into());
    }
    let parsed: JsonValue = serde_json::from_str(&resp_line)
        .map_err(|e| format!("invalid Node bridge JSON response: {e}"))?;
    if !parsed
        .get("ok")
        .and_then(JsonValue::as_bool)
        .unwrap_or(false)
    {
        let err = parsed
            .get("error")
            .and_then(JsonValue::as_str)
            .unwrap_or("Node.js execution error");
        return Err(err.to_string());
    }
    Ok(json_to_val(parsed.get("value").unwrap_or(&JsonValue::Null)))
}

// ── Electron-Class Desktop WebView Shell + Concurrent HTTP/SSE Server ──────

pub(crate) fn find_desktop_browser() -> Option<PathBuf> {
    let candidates: Vec<&str> = if cfg!(windows) {
        vec![
            r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
            r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
            r"C:\Program Files\Google\Chrome\Application\chrome.exe",
            r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
        ]
    } else {
        vec![
            "/usr/bin/microsoft-edge",
            "/usr/bin/google-chrome",
            "/usr/bin/chromium",
            "/usr/bin/chromium-browser",
        ]
    };
    candidates
        .into_iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
}

const KOVREK_IPC_CLIENT_JS: &str = r#"(function(){
  window.kovrek = {
    version: "12.0.0",
    async invoke(action, payload) {
      const r = await fetch("/__kovrek/invoke", {
        method: "POST",
        headers: {"Content-Type": "application/json"},
        body: JSON.stringify({ action: String(action || ""), payload: payload === undefined ? null : payload })
      });
      return r.json();
    },
    async health() {
      const r = await fetch("/__kovrek/health");
      return r.json();
    },
    async shutdown() {
      const r = await fetch("/__kovrek/shutdown", { method: "POST" });
      return r.json();
    }
  };
})();"#;

struct ChannelSseReader {
    ch_id: u64,
    buf: Vec<u8>,
    pos: usize,
    done: bool,
}

impl ChannelSseReader {
    fn new(ch_id: u64) -> Self {
        Self {
            ch_id,
            buf: Vec::new(),
            pos: 0,
            done: false,
        }
    }
}

impl Read for ChannelSseReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        while self.pos >= self.buf.len() {
            if self.done {
                return Ok(0);
            }
            let rx = {
                let Ok(guard) = channels().lock() else {
                    return Ok(0);
                };
                guard.get(&self.ch_id).map(|(_, r)| r.clone())
            };
            let Some(rx) = rx else {
                return Ok(0);
            };
            let maybe_val = {
                let Ok(lock) = rx.lock() else {
                    return Ok(0);
                };
                lock.recv_timeout(Duration::from_millis(120))
            };
            match maybe_val {
                Ok(val) => {
                    let j = val_to_json(&val);
                    let is_terminal = j
                        .get("kind")
                        .and_then(JsonValue::as_str)
                        .is_some_and(|k| k == "done" || k == "error")
                        || j.get("eof").and_then(JsonValue::as_bool).unwrap_or(false);
                    let payload = serde_json::to_string(&j).unwrap_or_else(|_| "null".into());
                    self.buf = format!("data: {payload}\n\n").into_bytes();
                    self.pos = 0;
                    if is_terminal {
                        self.done = true;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    self.buf = b": keepalive\n\n".to_vec();
                    self.pos = 0;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Ok(0);
                }
            }
        }
        let avail = &self.buf[self.pos..];
        let n = avail.len().min(out.len());
        out[..n].copy_from_slice(&avail[..n]);
        self.pos += n;
        Ok(n)
    }
}

fn static_content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|x| x.to_str()).unwrap_or("") {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "application/javascript; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        _ => "application/octet-stream",
    }
}

pub(crate) fn val_to_http_response(value: Val) -> Response<Box<dyn Read + Send>> {
    match value {
        Val::Map(pairs) => {
            if let Some(Val::Num(ch_num)) = map_lookup(&pairs, &Val::Str("stream_channel".into())) {
                let reader: Box<dyn Read + Send> = Box::new(ChannelSseReader::new(*ch_num as u64));
                let mut r = Response::new(StatusCode(200), vec![], reader, None, None);
                for (k, v) in [
                    ("Content-Type", "text/event-stream; charset=utf-8"),
                    ("Cache-Control", "no-cache"),
                ] {
                    if let Ok(h) = Header::from_bytes(k.as_bytes(), v.as_bytes()) {
                        r.add_header(h);
                    }
                }
                return r;
            }
            let has_http_keys = map_lookup(&pairs, &Val::Str("body".into())).is_some()
                || map_lookup(&pairs, &Val::Str("status".into())).is_some()
                || map_lookup(&pairs, &Val::Str("content_type".into())).is_some();
            if !has_http_keys {
                let map_val = Val::Map(pairs);
                let txt =
                    serde_json::to_string(&val_to_json(&map_val)).unwrap_or_else(|_| "{}".into());
                let mut r = Response::from_string(txt).boxed();
                if let Ok(h) = Header::from_bytes(
                    &b"Content-Type"[..],
                    &b"application/json; charset=utf-8"[..],
                ) {
                    r.add_header(h);
                }
                return r;
            }
            let status = match map_lookup(&pairs, &Val::Str("status".into())) {
                Some(Val::Num(n)) => *n as u16,
                _ => 200,
            };
            let content_type = match map_lookup(&pairs, &Val::Str("content_type".into())) {
                Some(Val::Str(s)) => s.clone(),
                _ => "application/json; charset=utf-8".into(),
            };
            let body = match map_lookup(&pairs, &Val::Str("body".into())) {
                Some(Val::Str(s)) => s.clone(),
                Some(other) => serde_json::to_string(&val_to_json(other)).unwrap_or_default(),
                None => String::new(),
            };
            let mut r = Response::from_string(body)
                .with_status_code(StatusCode(status))
                .boxed();
            if let Ok(h) = Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes()) {
                r.add_header(h);
            }
            if let Some(Val::Map(hdrs)) = map_lookup(&pairs, &Val::Str("headers".into())) {
                for (k, v) in hdrs {
                    if let (Val::Str(name), Val::Str(val)) = (k, v)
                        && let Ok(h) = Header::from_bytes(name.as_bytes(), val.as_bytes())
                    {
                        r.add_header(h);
                    }
                }
            }
            r
        }
        Val::Str(s) => {
            let mut r = Response::from_string(s).boxed();
            if let Ok(h) =
                Header::from_bytes(&b"Content-Type"[..], &b"text/plain; charset=utf-8"[..])
            {
                r.add_header(h);
            }
            r
        }
        other => {
            let txt = serde_json::to_string(&val_to_json(&other)).unwrap_or_else(|_| "null".into());
            let mut r = Response::from_string(txt).boxed();
            if let Ok(h) = Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]) {
                r.add_header(h);
            }
            r
        }
    }
}

fn resolve_safe_static(dir: &Path, req_path: &str) -> Option<PathBuf> {
    let rel = req_path.trim_start_matches('/');
    let rel_path = if rel.is_empty() {
        PathBuf::from("index.html")
    } else {
        safe_package_relative_path(rel).ok()?
    };
    let mut candidate = dir.join(rel_path);
    if candidate.is_dir() {
        candidate = candidate.join("index.html");
    }
    let canon_dir = dir.canonicalize().ok()?;
    let canon_file = candidate.canonicalize().ok()?;
    if canon_file.starts_with(&canon_dir) && canon_file.is_file() {
        Some(canon_file)
    } else {
        None
    }
}

fn handle_desktop_or_app_request(
    vm: &mut Vm,
    mut req: tiny_http::Request,
    handler: Option<&Val>,
    static_dir: Option<&Path>,
    inline_html: Option<&str>,
    stop_flag: &Arc<AtomicBool>,
) -> Result<(), String> {
    let full_url = req.url().to_string();
    let path = full_url.split('?').next().unwrap_or("/").to_string();
    let method = req.method().as_str().to_string();

    if path == "/kovrek-ipc.js" {
        let mut r = Response::from_string(KOVREK_IPC_CLIENT_JS).boxed();
        if let Ok(h) = Header::from_bytes(&b"Content-Type"[..], &b"application/javascript"[..]) {
            r.add_header(h);
        }
        return req.respond(r).map_err(|e| e.to_string());
    }
    if path == "/__kovrek/health" {
        let mut r = Response::from_string(r#"{"ok":true,"runtime":"kovrek-desktop"}"#).boxed();
        if let Ok(h) = Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]) {
            r.add_header(h);
        }
        return req.respond(r).map_err(|e| e.to_string());
    }
    if path == "/__kovrek/shutdown" && method == "POST" {
        stop_flag.store(true, Ordering::SeqCst);
        let r = Response::from_string(r#"{"ok":true,"shutting_down":true}"#).boxed();
        return req.respond(r).map_err(|e| e.to_string());
    }

    let mut body = String::new();
    let _ = req.as_reader().read_to_string(&mut body);
    let headers = Val::Map(
        req.headers()
            .iter()
            .map(|h| {
                (
                    Val::Str(h.field.as_str().to_string()),
                    Val::Str(h.value.as_str().to_string()),
                )
            })
            .collect(),
    );

    if path == "/__kovrek/invoke" && method == "POST" {
        let Some(h) = handler else {
            let r = Response::from_string(r#"{"ok":false,"error":"no IPC handler bound"}"#)
                .with_status_code(StatusCode(404))
                .boxed();
            return req.respond(r).map_err(|e| e.to_string());
        };
        let parsed: JsonValue = serde_json::from_str(&body).unwrap_or(JsonValue::Null);
        let action = parsed
            .get("action")
            .and_then(JsonValue::as_str)
            .unwrap_or("")
            .to_string();
        let payload = json_to_val(parsed.get("payload").unwrap_or(&JsonValue::Null));
        let ev = Val::Map(vec![
            (Val::Str("kind".into()), Val::Str("ipc".into())),
            (Val::Str("type".into()), Val::Str("ipc".into())),
            (Val::Str("action".into()), Val::Str(action)),
            (Val::Str("payload".into()), payload),
        ]);
        let resp = match vm.invoke_callable_sync(h, vec![ev]) {
            Ok(v) => val_to_http_response(Val::Map(vec![
                (Val::Str("status".into()), Val::Num(200.0)),
                (
                    Val::Str("content_type".into()),
                    Val::Str("application/json; charset=utf-8".into()),
                ),
                (
                    Val::Str("body".into()),
                    Val::Map(vec![
                        (Val::Str("ok".into()), Val::Bool(true)),
                        (Val::Str("result".into()), v),
                    ]),
                ),
            ])),
            Err(e) => Response::from_string(json!({ "ok": false, "error": e }).to_string())
                .with_status_code(StatusCode(500))
                .boxed(),
        };
        return req.respond(resp).map_err(|e| e.to_string());
    }

    if let Some(dir) = static_dir
        && method == "GET"
        && let Some(file_path) = resolve_safe_static(dir, &path)
        && let Ok(bytes) = fs::read(&file_path)
    {
        let mut r = Response::from_data(bytes).boxed();
        if let Ok(h) = Header::from_bytes(
            &b"Content-Type"[..],
            static_content_type(&file_path).as_bytes(),
        ) {
            r.add_header(h);
        }
        return req.respond(r).map_err(|e| e.to_string());
    }

    if let Some(html) = inline_html
        && method == "GET"
        && (path == "/" || path == "/index.html")
    {
        let mut r = Response::from_string(html.to_string()).boxed();
        if let Ok(h) = Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]) {
            r.add_header(h);
        }
        return req.respond(r).map_err(|e| e.to_string());
    }

    if let Some(h) = handler {
        let req_val = Val::Map(vec![
            (Val::Str("kind".into()), Val::Str("http".into())),
            (Val::Str("method".into()), Val::Str(method)),
            (Val::Str("path".into()), Val::Str(path)),
            (Val::Str("url".into()), Val::Str(full_url.clone())),
            (Val::Str("body".into()), Val::Str(body)),
            (
                Val::Str("query".into()),
                crate::server::query_params(&full_url),
            ),
            (Val::Str("params".into()), Val::Map(vec![])),
            (Val::Str("headers".into()), headers),
        ]);
        let resp = match vm.invoke_callable_sync(h, vec![req_val]) {
            Ok(v) => val_to_http_response(v),
            Err(e) => Response::from_string(format!("Kovrek handler error: {e}"))
                .with_status_code(StatusCode(500))
                .boxed(),
        };
        return req.respond(resp).map_err(|e| e.to_string());
    }

    req.respond(
        Response::from_string("404 Not Found")
            .with_status_code(StatusCode(404))
            .boxed(),
    )
    .map_err(|e| e.to_string())
}

struct AppPoolConfig {
    handler: Option<Val>,
    static_dir: Option<PathBuf>,
    inline_html: Option<String>,
    stop_flag: Arc<AtomicBool>,
    workers: usize,
    max_requests: Option<usize>,
}

fn spawn_app_worker_pool(
    server: Arc<Server>,
    vm: &Vm,
    cfg: AppPoolConfig,
) -> Vec<thread::JoinHandle<()>> {
    let handled = Arc::new(AtomicU64::new(0));
    (0..cfg.workers)
        .map(|_| {
            let srv = server.clone();
            let mut wvm = vm.clone_worker();
            let h = cfg.handler.clone();
            let sd = cfg.static_dir.clone();
            let ih = cfg.inline_html.clone();
            let stop = cfg.stop_flag.clone();
            let limit_opt = cfg.max_requests;
            let cnt = handled.clone();
            thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    if let Some(limit) = limit_opt
                        && cnt.load(Ordering::SeqCst) >= limit as u64
                    {
                        stop.store(true, Ordering::SeqCst);
                        break;
                    }
                    match srv.recv_timeout(Duration::from_millis(120)) {
                        Ok(Some(req)) => {
                            let c = cnt.fetch_add(1, Ordering::SeqCst) + 1;
                            let _ = handle_desktop_or_app_request(
                                &mut wvm,
                                req,
                                h.as_ref(),
                                sd.as_deref(),
                                ih.as_deref(),
                                &stop,
                            );
                            if let Some(limit) = limit_opt
                                && c >= limit as u64
                            {
                                stop.store(true, Ordering::SeqCst);
                                break;
                            }
                        }
                        Ok(None) => continue,
                        Err(_) => break,
                    }
                }
            })
        })
        .collect()
}

pub(crate) fn run_desktop_app(vm: &mut Vm, args: Vec<Val>) -> Result<Val, String> {
    require_cap("net")?;
    if args.is_empty() || args.len() > 2 {
        return Err("desktop_app expects (spec_map[, handler_fn])".into());
    }
    let Val::Map(pairs) = &args[0] else {
        return Err("desktop_app first argument must be spec Map".into());
    };
    let handler = args.get(1).cloned();
    let port = match map_lookup(pairs, &Val::Str("port".into())) {
        Some(Val::Num(n)) if *n > 0.0 => *n as u16,
        _ => 0,
    };
    let width = match map_lookup(pairs, &Val::Str("width".into())) {
        Some(Val::Num(n)) => (*n as i32).clamp(320, 2560),
        _ => 1240,
    };
    let height = match map_lookup(pairs, &Val::Str("height".into())) {
        Some(Val::Num(n)) => (*n as i32).clamp(240, 1600),
        _ => 820,
    };
    let workers = apply_map_usize(pairs, "workers", 4).clamp(1, 16);
    let max_requests = map_lookup(pairs, &Val::Str("max_requests".into())).and_then(|v| match v {
        Val::Num(n) if *n >= 1.0 => Some(*n as usize),
        _ => None,
    });
    let headless = match map_lookup(pairs, &Val::Str("headless".into())) {
        Some(Val::Bool(b)) => *b,
        _ => false,
    } || env::var("KOVREK_DESKTOP_HEADLESS").ok().as_deref() == Some("1");

    let static_dir = match map_lookup(pairs, &Val::Str("dir".into())) {
        Some(Val::Str(s)) => Some(PathBuf::from(s)),
        _ => None,
    };
    let inline_html = match map_lookup(pairs, &Val::Str("html".into())) {
        Some(Val::Str(s)) => Some(s.clone()),
        _ => None,
    };
    let external_url = match map_lookup(pairs, &Val::Str("url".into())) {
        Some(Val::Str(s)) => Some(s.clone()),
        _ => None,
    };

    let server = Arc::new(Server::http(format!("127.0.0.1:{port}")).map_err(|e| e.to_string())?);
    let bound_port = server
        .server_addr()
        .to_ip()
        .map(|a| a.port())
        .unwrap_or(port);
    let target_url = external_url.unwrap_or_else(|| format!("http://127.0.0.1:{bound_port}/"));
    let stop_flag = Arc::new(AtomicBool::new(false));
    let handles = spawn_app_worker_pool(
        server.clone(),
        vm,
        AppPoolConfig {
            handler,
            static_dir,
            inline_html,
            stop_flag: stop_flag.clone(),
            workers,
            max_requests,
        },
    );

    if let Some(on_ready) = map_lookup(pairs, &Val::Str("on_ready".into())).cloned()
        && vm.is_callable_val(&on_ready)
        && let Err(e) = vm.invoke_callable_sync(
            &on_ready,
            vec![Val::Map(vec![
                (Val::Str("url".into()), Val::Str(target_url.clone())),
                (Val::Str("port".into()), Val::Num(f64::from(bound_port))),
            ])],
        )
    {
        stop_flag.store(true, Ordering::SeqCst);
        for h in handles {
            let _ = h.join();
        }
        return Err(e);
    }

    let mut launched_browser = false;
    if !headless {
        if let Some(browser) = find_desktop_browser() {
            let profile_dir = env::temp_dir().join(format!(
                "kovrek_webview_{}_{}",
                std::process::id(),
                bound_port
            ));
            let _ = fs::create_dir_all(&profile_dir);
            if let Ok(mut child) = Command::new(browser)
                .arg(format!("--app={target_url}"))
                .arg(format!("--window-size={width},{height}"))
                .arg(format!("--user-data-dir={}", profile_dir.display()))
                .arg("--no-first-run")
                .spawn()
            {
                launched_browser = true;
                let _ = child.wait();
                stop_flag.store(true, Ordering::SeqCst);
            }
            let _ = fs::remove_dir_all(&profile_dir);
        }
    } else if max_requests.is_none() {
        stop_flag.store(true, Ordering::SeqCst);
    }

    for h in handles {
        let _ = h.join();
    }

    Ok(Val::Map(vec![
        (Val::Str("ok".into()), Val::Bool(true)),
        (Val::Str("url".into()), Val::Str(target_url)),
        (Val::Str("port".into()), Val::Num(f64::from(bound_port))),
        (Val::Str("headless".into()), Val::Bool(headless)),
        (
            Val::Str("launched_browser".into()),
            Val::Bool(launched_browser),
        ),
    ]))
}

pub(crate) fn run_app_serve(vm: &mut Vm, args: Vec<Val>) -> Result<Val, String> {
    require_cap("net")?;
    if args.is_empty() || args.len() > 2 {
        return Err("app_serve expects (handler_fn[, options_map])".into());
    }
    let handler = args[0].clone();
    let opts = match args.get(1) {
        Some(Val::Map(pairs)) => pairs.clone(),
        None => Vec::new(),
        _ => return Err("app_serve second argument must be Map".into()),
    };
    let port = match map_lookup(&opts, &Val::Str("port".into())) {
        Some(Val::Num(n)) => *n as u16,
        _ => 8080,
    };
    let workers = apply_map_usize(&opts, "workers", 4).clamp(1, 16);
    let max_requests = map_lookup(&opts, &Val::Str("max_requests".into())).and_then(|v| match v {
        Val::Num(n) if *n >= 1.0 => Some(*n as usize),
        _ => None,
    });
    let static_dir = match map_lookup(&opts, &Val::Str("static_dir".into())) {
        Some(Val::Str(s)) => Some(PathBuf::from(s)),
        _ => None,
    };

    let server = Arc::new(Server::http(format!("127.0.0.1:{port}")).map_err(|e| e.to_string())?);
    let bound_port = server
        .server_addr()
        .to_ip()
        .map(|a| a.port())
        .unwrap_or(port);
    let stop_flag = Arc::new(AtomicBool::new(false));
    let handles = spawn_app_worker_pool(
        server,
        vm,
        AppPoolConfig {
            handler: Some(handler),
            static_dir,
            inline_html: None,
            stop_flag: stop_flag.clone(),
            workers,
            max_requests,
        },
    );
    if let Some(on_ready) = map_lookup(&opts, &Val::Str("on_ready".into())).cloned()
        && vm.is_callable_val(&on_ready)
        && let Err(e) = vm.invoke_callable_sync(&on_ready, vec![Val::Num(f64::from(bound_port))])
    {
        stop_flag.store(true, Ordering::SeqCst);
        for h in handles {
            let _ = h.join();
        }
        return Err(e);
    }
    for h in handles {
        let _ = h.join();
    }
    Ok(Val::Map(vec![
        (Val::Str("ok".into()), Val::Bool(true)),
        (Val::Str("port".into()), Val::Num(f64::from(bound_port))),
    ]))
}

// ── Unified Polyglot & Universal Architecture Builtin Dispatcher ───────────

pub(crate) fn call_builtin_universal_arch(
    vm: &mut Vm,
    name: &str,
    args: Vec<Val>,
) -> Result<Option<Val>, String> {
    let v = match name {
        "fs_mkdir" => {
            require_cap("fs.write")?;
            if args.len() != 1 {
                return Err("fs_mkdir expects path".into());
            }
            let p = str_arg(&args[0], "path")?;
            fs::create_dir_all(&p).map_err(|e| e.to_string())?;
            Val::Bool(true)
        }
        "fs_list" => {
            require_cap("fs.read")?;
            if args.len() != 1 {
                return Err("fs_list expects dir".into());
            }
            let p = str_arg(&args[0], "dir")?;
            let mut entries: Vec<fs::DirEntry> = fs::read_dir(&p)
                .map_err(|e| e.to_string())?
                .filter_map(Result::ok)
                .collect();
            entries.sort_by_key(|e| e.file_name());
            let mut out = Vec::new();
            for e in entries {
                let path = e.path();
                let meta = e.metadata().ok();
                let is_dir = meta.as_ref().is_some_and(fs::Metadata::is_dir);
                let is_file = meta.as_ref().is_some_and(fs::Metadata::is_file);
                let size = meta.as_ref().map(|m| m.len() as f64).unwrap_or(0.0);
                out.push(Val::Map(vec![
                    (
                        Val::Str("name".into()),
                        Val::Str(e.file_name().to_string_lossy().to_string()),
                    ),
                    (
                        Val::Str("path".into()),
                        Val::Str(path.to_string_lossy().to_string()),
                    ),
                    (Val::Str("is_dir".into()), Val::Bool(is_dir)),
                    (Val::Str("is_file".into()), Val::Bool(is_file)),
                    (Val::Str("size".into()), Val::Num(size)),
                ]));
            }
            Val::List(out)
        }
        "fs_walk" => {
            require_cap("fs.read")?;
            if args.is_empty() || args.len() > 2 {
                return Err("fs_walk expects (dir[, max_depth | options_map])".into());
            }
            let root = PathBuf::from(str_arg(&args[0], "dir")?);
            let mut max_depth: Option<usize> = None;
            let opts = match args.get(1) {
                Some(Val::Map(p)) => p.clone(),
                Some(Val::Num(n)) => {
                    max_depth = Some((*n).max(0.0) as usize);
                    Vec::new()
                }
                Some(Val::Null) | None => Vec::new(),
                _ => return Err("fs_walk second argument must be Map or Number".into()),
            };
            if max_depth.is_none()
                && let Some(Val::Num(n)) = map_lookup(&opts, &Val::Str("max_depth".into()))
            {
                max_depth = Some((*n).max(0.0) as usize);
            }
            let skip_dirs = apply_map_str_list(&opts, "skip_dirs").unwrap_or_else(|| {
                vec![
                    ".git".into(),
                    "node_modules".into(),
                    "target".into(),
                    "dist".into(),
                    "__pycache__".into(),
                ]
            });
            let exts = apply_map_str_list(&opts, "extensions");
            let max_files = apply_map_usize(&opts, "max_files", 256);
            let max_bytes = apply_map_u64(&opts, "max_bytes", 1_048_576);
            let mut out = Vec::new();
            if root.is_file() {
                let meta = fs::metadata(&root).map_err(|e| e.to_string())?;
                out.push(Val::Map(vec![
                    (
                        Val::Str("name".into()),
                        Val::Str(
                            root.file_name()
                                .map(|n| n.to_string_lossy().to_string())
                                .unwrap_or_default(),
                        ),
                    ),
                    (
                        Val::Str("path".into()),
                        Val::Str(root.to_string_lossy().to_string()),
                    ),
                    (
                        Val::Str("rel_path".into()),
                        Val::Str(
                            root.file_name()
                                .map(|n| n.to_string_lossy().to_string())
                                .unwrap_or_default(),
                        ),
                    ),
                    (Val::Str("size".into()), Val::Num(meta.len() as f64)),
                ]));
            } else {
                let walk_cfg = WalkConfig {
                    root: &root,
                    max_depth,
                    skip_dirs: &skip_dirs,
                    exts: exts.as_deref(),
                    max_files,
                    max_bytes,
                };
                walk_dir_rec(&walk_cfg, &root, 0, &mut out)?;
            }
            Val::List(out)
        }
        "fs_remove" => {
            require_cap("fs.write")?;
            if args.len() != 1 {
                return Err("fs_remove expects path".into());
            }
            let raw = str_arg(&args[0], "path")?;
            let p = Path::new(&raw);
            if !p.exists() {
                Val::Bool(false)
            } else if p.is_dir() {
                fs::remove_dir_all(p).map_err(|e| e.to_string())?;
                Val::Bool(true)
            } else {
                fs::remove_file(p).map_err(|e| e.to_string())?;
                Val::Bool(true)
            }
        }
        "fs_stat" => {
            require_cap("fs.read")?;
            if args.len() != 1 {
                return Err("fs_stat expects path".into());
            }
            let raw = str_arg(&args[0], "path")?;
            let p = Path::new(&raw);
            match fs::metadata(p) {
                Ok(m) => Val::Map(vec![
                    (Val::Str("exists".into()), Val::Bool(true)),
                    (Val::Str("is_file".into()), Val::Bool(m.is_file())),
                    (Val::Str("is_dir".into()), Val::Bool(m.is_dir())),
                    (Val::Str("size".into()), Val::Num(m.len() as f64)),
                    (
                        Val::Str("readonly".into()),
                        Val::Bool(m.permissions().readonly()),
                    ),
                    (
                        Val::Str("modified_ms".into()),
                        Val::Num(modified_epoch_ms(&m)),
                    ),
                ]),
                Err(_) => Val::Map(vec![
                    (Val::Str("exists".into()), Val::Bool(false)),
                    (Val::Str("is_file".into()), Val::Bool(false)),
                    (Val::Str("is_dir".into()), Val::Bool(false)),
                    (Val::Str("size".into()), Val::Num(0.0)),
                ]),
            }
        }
        "fs_read_bytes" => {
            require_cap("fs.read")?;
            if args.len() != 1 {
                return Err("fs_read_bytes expects path".into());
            }
            let p = str_arg(&args[0], "path")?;
            let bytes = fs::read(&p).map_err(|e| e.to_string())?;
            Val::List(bytes.into_iter().map(|b| Val::Num(f64::from(b))).collect())
        }
        "fs_write_bytes" => {
            require_cap("fs.write")?;
            if args.len() != 2 {
                return Err("fs_write_bytes expects (path, bytes_list)".into());
            }
            let p = str_arg(&args[0], "path")?;
            let Val::List(items) = &args[1] else {
                return Err("bytes must be List".into());
            };
            let mut raw = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    Val::Num(n) if *n >= 0.0 && *n <= 255.0 => raw.push(*n as u8),
                    _ => return Err("byte entries must be numbers in 0..=255".into()),
                }
            }
            let written = raw.len();
            fs::write(&p, raw).map_err(|e| e.to_string())?;
            Val::Num(written as f64)
        }
        "fs_read_base64" => {
            require_cap("fs.read")?;
            if args.len() != 1 {
                return Err("fs_read_base64 expects path".into());
            }
            let p = str_arg(&args[0], "path")?;
            let bytes = fs::read(&p).map_err(|e| e.to_string())?;
            Val::Str(base64::engine::general_purpose::STANDARD.encode(bytes))
        }
        "env_set" => {
            require_cap("process")?;
            if args.len() != 2 {
                return Err("env_set expects (key, value)".into());
            }
            let k = str_arg(&args[0], "key")?;
            let v = str_arg(&args[1], "value")?;
            // Safety: explicit user call to env_set builtin.
            unsafe { env::set_var(k, v) };
            Val::Bool(true)
        }
        "env_vars" => {
            require_cap("env")?;
            let mut pairs: Vec<(String, String)> = env::vars().collect();
            pairs.sort_by(|a, b| a.0.cmp(&b.0));
            Val::Map(
                pairs
                    .into_iter()
                    .map(|(k, v)| (Val::Str(k), Val::Str(v)))
                    .collect(),
            )
        }
        "proc_spawn" => {
            require_cap("process")?;
            if args.is_empty() || args.len() > 3 {
                return Err("proc_spawn expects (cmd[, args_list[, opts_map]])".into());
            }
            let cmd = str_arg(&args[0], "cmd")?;
            let mut cmd_builder = Command::new(&cmd);
            if let Some(Val::List(xs)) = args.get(1) {
                cmd_builder.args(xs.iter().map(Val::show));
            }
            if let Some(Val::Map(opts)) = args.get(2) {
                if let Some(Val::Str(cwd)) = map_lookup(opts, &Val::Str("cwd".into()))
                    && !cwd.trim().is_empty()
                {
                    cmd_builder.current_dir(cwd);
                }
                if let Some(Val::Map(envs)) = map_lookup(opts, &Val::Str("env".into())) {
                    for (ek, ev) in envs {
                        cmd_builder.env(ek.show(), ev.show());
                    }
                }
            }
            cmd_builder
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = cmd_builder.spawn().map_err(|e| e.to_string())?;
            let stdin = child.stdin.take();
            let stdout = child.stdout.take();
            let stderr = child.stderr.take();
            let out_buf = Arc::new(Mutex::new(VecDeque::new()));
            let err_buf = Arc::new(Mutex::new(VecDeque::new()));
            let running = Arc::new(AtomicBool::new(true));
            let exit_code = Arc::new(Mutex::new(None));

            if let Some(so) = stdout {
                let ob = out_buf.clone();
                let run_flag = running.clone();
                thread::spawn(move || {
                    for line in BufReader::new(so).lines().map_while(Result::ok) {
                        push_ring_line(&ob, line);
                    }
                    run_flag.store(false, Ordering::SeqCst);
                });
            }
            if let Some(se) = stderr {
                let eb = err_buf.clone();
                thread::spawn(move || {
                    for line in BufReader::new(se).lines().map_while(Result::ok) {
                        push_ring_line(&eb, line);
                    }
                });
            }
            let id = PROC_IDS.fetch_add(1, Ordering::Relaxed);
            procs()
                .lock()
                .map_err(|_| "process table poisoned")?
                .insert(
                    id,
                    ManagedProc {
                        child,
                        stdin,
                        out_buf,
                        err_buf,
                        running,
                        exit_code,
                    },
                );
            Val::Num(id as f64)
        }
        "proc_read" => {
            require_cap("process")?;
            if args.len() != 1 {
                return Err("proc_read expects proc_id".into());
            }
            let id = match args[0] {
                Val::Num(n) => n as u64,
                _ => return Err("proc_id must be Number".into()),
            };
            let mut table = procs().lock().map_err(|_| "process table poisoned")?;
            let mp = table.get_mut(&id).ok_or(format!("unknown process {id}"))?;
            let code = match mp.child.try_wait() {
                Ok(Some(st)) => {
                    let c = st.code().unwrap_or(-1);
                    if let Ok(mut ec) = mp.exit_code.lock() {
                        *ec = Some(c);
                    }
                    mp.running.store(false, Ordering::SeqCst);
                    Some(c)
                }
                Ok(None) => None,
                Err(_) => None,
            };
            let out_lines = drain_ring_lines(&mp.out_buf);
            let err_lines = drain_ring_lines(&mp.err_buf);
            let out = out_lines.join("\n");
            let err = err_lines.join("\n");
            let is_running = code.is_none() && mp.running.load(Ordering::SeqCst);
            Val::Map(vec![
                (Val::Str("out".into()), Val::Str(out)),
                (Val::Str("err".into()), Val::Str(err)),
                (
                    Val::Str("out_lines".into()),
                    Val::List(out_lines.into_iter().map(Val::Str).collect()),
                ),
                (
                    Val::Str("err_lines".into()),
                    Val::List(err_lines.into_iter().map(Val::Str).collect()),
                ),
                (Val::Str("running".into()), Val::Bool(is_running)),
                (
                    Val::Str("exit_code".into()),
                    code.map(|c| Val::Num(f64::from(c))).unwrap_or(Val::Null),
                ),
            ])
        }
        "proc_write" => {
            require_cap("process")?;
            if args.len() != 2 {
                return Err("proc_write expects (proc_id, text)".into());
            }
            let id = match args[0] {
                Val::Num(n) => n as u64,
                _ => return Err("proc_id must be Number".into()),
            };
            let text = str_arg(&args[1], "text")?;
            let mut table = procs().lock().map_err(|_| "process table poisoned")?;
            let mp = table.get_mut(&id).ok_or(format!("unknown process {id}"))?;
            let stdin = mp
                .stdin
                .as_mut()
                .ok_or(format!("process {id} stdin already closed"))?;
            stdin
                .write_all(text.as_bytes())
                .map_err(|e| e.to_string())?;
            stdin.flush().map_err(|e| e.to_string())?;
            Val::Bool(true)
        }
        "proc_status" => {
            require_cap("process")?;
            if args.len() != 1 {
                return Err("proc_status expects proc_id".into());
            }
            let id = match args[0] {
                Val::Num(n) => n as u64,
                _ => return Err("proc_id must be Number".into()),
            };
            let mut table = procs().lock().map_err(|_| "process table poisoned")?;
            let mp = table.get_mut(&id).ok_or(format!("unknown process {id}"))?;
            let pid = mp.child.id();
            let code = match mp.child.try_wait() {
                Ok(Some(st)) => {
                    let c = st.code().unwrap_or(-1);
                    if let Ok(mut ec) = mp.exit_code.lock() {
                        *ec = Some(c);
                    }
                    mp.running.store(false, Ordering::SeqCst);
                    Some(c)
                }
                Ok(None) => mp.exit_code.lock().ok().and_then(|g| *g),
                Err(_) => None,
            };
            Val::Map(vec![
                (Val::Str("pid".into()), Val::Num(f64::from(pid))),
                (Val::Str("running".into()), Val::Bool(code.is_none())),
                (
                    Val::Str("exit_code".into()),
                    code.map(|c| Val::Num(f64::from(c))).unwrap_or(Val::Null),
                ),
            ])
        }
        "proc_kill" => {
            require_cap("process")?;
            if args.len() != 1 {
                return Err("proc_kill expects proc_id".into());
            }
            let id = match args[0] {
                Val::Num(n) => n as u64,
                _ => return Err("proc_id must be Number".into()),
            };
            let mut table = procs().lock().map_err(|_| "process table poisoned")?;
            let mut mp = table.remove(&id).ok_or(format!("unknown process {id}"))?;
            let _ = mp.stdin.take();
            let _ = mp.child.kill();
            let status = mp.child.wait().ok().and_then(|s| s.code());
            Val::Map(vec![
                (Val::Str("killed".into()), Val::Bool(true)),
                (
                    Val::Str("exit_code".into()),
                    status.map(|c| Val::Num(f64::from(c))).unwrap_or(Val::Null),
                ),
            ])
        }
        "node_available" => Val::Bool(find_node_binary().is_some()),
        "node_info" => {
            let bin = find_node_binary();
            let version = bin
                .as_ref()
                .and_then(|b| Command::new(b).arg("--version").output().ok())
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_default();
            let npm_bin = find_npm_binary().unwrap_or_default();
            Val::Map(vec![
                (Val::Str("available".into()), Val::Bool(bin.is_some())),
                (
                    Val::Str("binary".into()),
                    Val::Str(
                        bin.map(|p| p.to_string_lossy().to_string())
                            .unwrap_or_default(),
                    ),
                ),
                (Val::Str("version".into()), Val::Str(version)),
                (Val::Str("npm_binary".into()), Val::Str(npm_bin)),
            ])
        }
        "node_eval" => {
            require_cap("process")?;
            if args.len() != 1 {
                return Err("node_eval expects JS code String".into());
            }
            let code = str_arg(&args[0], "code")?;
            node_rpc(json!({ "op": "eval", "code": code }))?
        }
        "node_require" => {
            require_cap("process")?;
            if args.len() != 1 {
                return Err("node_require expects module String".into());
            }
            let module = str_arg(&args[0], "module")?;
            node_rpc(json!({ "op": "require", "module": module }))?
        }
        "node_call" => {
            require_cap("process")?;
            if args.len() < 2 || args.len() > 3 {
                return Err("node_call expects (module, fn_name[, args_list])".into());
            }
            let module = str_arg(&args[0], "module")?;
            let fn_name = str_arg(&args[1], "fn_name")?;
            let call_args: Vec<JsonValue> = match args.get(2) {
                Some(Val::List(xs)) => xs.iter().map(val_to_json).collect(),
                None => Vec::new(),
                _ => return Err("node_call args must be List".into()),
            };
            node_rpc(json!({
                "op": "call",
                "module": module,
                "fn": fn_name,
                "args": call_args
            }))?
        }
        "npm_run" => {
            require_cap("process")?;
            if args.is_empty() || args.len() > 2 {
                return Err("npm_run expects (args_list[, cwd])".into());
            }
            let npm = find_npm_binary().ok_or("npm CLI not found in PATH")?;
            let Val::List(xs) = &args[0] else {
                return Err("npm_run args must be List".into());
            };
            let mut cmd = Command::new(&npm);
            cmd.args(xs.iter().map(Val::show));
            if let Some(Val::Str(cwd)) = args.get(1)
                && !cwd.trim().is_empty()
            {
                cmd.current_dir(cwd);
            }
            let t0 = Instant::now();
            let out = cmd.output().map_err(|e| e.to_string())?;
            Val::Map(vec![
                (
                    Val::Str("code".into()),
                    Val::Num(f64::from(out.status.code().unwrap_or(-1))),
                ),
                (
                    Val::Str("out".into()),
                    Val::Str(String::from_utf8_lossy(&out.stdout).to_string()),
                ),
                (
                    Val::Str("err".into()),
                    Val::Str(String::from_utf8_lossy(&out.stderr).to_string()),
                ),
                (
                    Val::Str("elapsed_ms".into()),
                    Val::Num(t0.elapsed().as_secs_f64() * 1000.0),
                ),
            ])
        }
        "desktop_app" => run_desktop_app(vm, args)?,
        "app_serve" => run_app_serve(vm, args)?,
        "runtime_info" => {
            let py_ok = crate::py_bridge::py_available();
            let node_ok = find_node_binary().is_some();
            let webview_ok = find_desktop_browser().is_some();
            let jit_ok = crate::jit::jit_available();
            let gpu_ok = crate::gpu::gpu_available();
            let bash_ok = crate::term_ui::find_git_bash().is_some();
            Val::Map(vec![
                (
                    Val::Str("kovrek_version".into()),
                    Val::Str(crate::VERSION.into()),
                ),
                (Val::Str("os".into()), Val::Str(env::consts::OS.into())),
                (Val::Str("arch".into()), Val::Str(env::consts::ARCH.into())),
                (
                    Val::Str("cpu_count".into()),
                    Val::Num(
                        thread::available_parallelism()
                            .map(|n| n.get())
                            .unwrap_or(1) as f64,
                    ),
                ),
                (
                    Val::Str("portable_core".into()),
                    Val::Bool(!cfg!(feature = "native-services")),
                ),
                (
                    Val::Str("bridges".into()),
                    Val::Map(vec![
                        (Val::Str("python".into()), Val::Bool(py_ok)),
                        (Val::Str("node".into()), Val::Bool(node_ok)),
                        (Val::Str("desktop_webview".into()), Val::Bool(webview_ok)),
                        (Val::Str("win32_gui".into()), Val::Bool(cfg!(windows))),
                        (Val::Str("jit".into()), Val::Bool(jit_ok)),
                        (Val::Str("gpu".into()), Val::Bool(gpu_ok)),
                        (Val::Str("wasm".into()), Val::Bool(true)),
                        (Val::Str("ffi".into()), Val::Bool(true)),
                        (Val::Str("git_bash".into()), Val::Bool(bash_ok)),
                        (Val::Str("powershell".into()), Val::Bool(cfg!(windows))),
                    ]),
                ),
            ])
        }
        "bridge_call" => {
            if args.len() != 3 {
                return Err("bridge_call expects (runtime, target, payload)".into());
            }
            let rt = str_arg(&args[0], "runtime")?.to_ascii_lowercase();
            let target = str_arg(&args[1], "target")?;
            let payload = &args[2];
            match rt.as_str() {
                "node" | "js" => {
                    require_cap("process")?;
                    match target.as_str() {
                        "eval" => node_rpc(json!({ "op": "eval", "code": payload.show() }))?,
                        "require" => {
                            node_rpc(json!({ "op": "require", "module": payload.show() }))?
                        }
                        _ => {
                            let (mod_name, fn_name) = target
                                .split_once(':')
                                .or_else(|| target.split_once('.'))
                                .ok_or(
                                    "node bridge target must be 'eval', 'require', or 'module:fn'",
                                )?;
                            let call_args: Vec<JsonValue> = match payload {
                                Val::List(xs) => xs.iter().map(val_to_json).collect(),
                                Val::Null => Vec::new(),
                                other => vec![val_to_json(other)],
                            };
                            node_rpc(json!({
                                "op": "call",
                                "module": mod_name,
                                "fn": fn_name,
                                "args": call_args
                            }))?
                        }
                    }
                }
                "shell" => {
                    require_cap("process")?;
                    crate::term_ui::builtin_shell_val(
                        crate::term_ui::ShellKind::System,
                        &payload.show(),
                    )?
                }
                "powershell" | "ps" => {
                    require_cap("process")?;
                    crate::term_ui::builtin_shell_val(
                        crate::term_ui::ShellKind::PowerShell,
                        &payload.show(),
                    )?
                }
                "bash" => {
                    require_cap("process")?;
                    crate::term_ui::builtin_shell_val(
                        crate::term_ui::ShellKind::GitBash,
                        &payload.show(),
                    )?
                }
                "verify" => {
                    let rep = verify_source_strict(&payload.show())?;
                    json_to_val(&rep)
                }
                other => return Err(format!("bridge_call: unknown runtime '{other}'")),
            }
        }
        _ => return Ok(None),
    };
    Ok(Some(v))
}
