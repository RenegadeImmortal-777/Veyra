// Persistent Python 3 JSON-RPC bridge for the native Horizon engine.
// Uses only std::process and serde_json so `--no-default-features` stays intact.

use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{Mutex, OnceLock},
};

use serde_json::{Value as JsonValue, json};

use crate::value::{Val, json_to_val};

const WORKER_SCRIPT: &str = r#"
import importlib, json, sys

_PROTO_OUT = sys.stdout
sys.stdout = sys.stderr

_OBJS = {}
_NEXT_ID = 1
_ENV = {"__builtins__": __builtins__}

def _alloc(obj):
    global _NEXT_ID
    oid = _NEXT_ID
    _NEXT_ID += 1
    _OBJS[oid] = obj
    return {"__py_handle__": oid, "__py_type__": type(obj).__name__}

def _to_wire(v):
    if v is None or isinstance(v, (bool, int, float, str)):
        return v
    if isinstance(v, (list, tuple)):
        return [_to_wire(x) for x in v]
    if type(v) is dict:
        out = {}
        for k, val in v.items():
            out[str(k)] = _to_wire(val)
        if "__py_handle__" in out and len(out) == 2 and "__py_type__" in out:
            return _alloc(v)
        return out
    return _alloc(v)

def _from_wire(x):
    if isinstance(x, list):
        return [_from_wire(v) for v in x]
    if isinstance(x, dict):
        if "__py_ref__" in x and len(x) == 1:
            oid = int(x["__py_ref__"])
            if oid not in _OBJS:
                raise KeyError(f"dangling python handle {oid}")
            return _OBJS[oid]
        return {k: _from_wire(v) for k, v in x.items()}
    return x

def _dispatch(req):
    op = req["op"]
    if op == "import":
        return _to_wire(importlib.import_module(req["name"]))
    if op == "call":
        fn = _from_wire(req["fn"])
        args = _from_wire(req.get("args", []))
        kwargs = _from_wire(req.get("kwargs", {})) or {}
        if isinstance(kwargs, dict):
            return _to_wire(fn(*args, **kwargs))
        return _to_wire(fn(*args))
    if op == "call_method":
        obj = _from_wire(req["obj"])
        method = req["method"]
        args = _from_wire(req.get("args", []))
        kwargs = _from_wire(req.get("kwargs", {})) or {}
        target = getattr(obj, method)
        return _to_wire(target(*args, **kwargs))
    if op == "get":
        obj = _from_wire(req["obj"])
        return _to_wire(getattr(obj, req["attr"]))
    if op == "set":
        obj = _from_wire(req["obj"])
        val = _from_wire(req["val"])
        setattr(obj, req["attr"], val)
        return _to_wire(val)
    if op == "hasattr":
        obj = _from_wire(req["obj"])
        return bool(hasattr(obj, req["attr"]))
    if op == "getitem":
        obj = _from_wire(req["obj"])
        key = _from_wire(req["key"])
        return _to_wire(obj[key])
    if op == "setitem":
        obj = _from_wire(req["obj"])
        key = _from_wire(req["key"])
        val = _from_wire(req["val"])
        obj[key] = val
        return _to_wire(val)
    if op == "dir":
        obj = _from_wire(req["obj"])
        return [str(x) for x in dir(obj)]
    if op == "str":
        obj = _from_wire(req["obj"])
        return str(obj)
    if op == "repr":
        obj = _from_wire(req["obj"])
        return repr(obj)
    if op == "eval":
        expr = req["expr"]
        loc = _from_wire(req.get("locals", {})) or {}
        if not isinstance(loc, dict):
            raise TypeError("py_eval locals must be a dict/Map")
        return _to_wire(eval(expr, _ENV, loc))
    if op == "exec":
        exec(req["code"], _ENV)
        return None
    if op == "release":
        _OBJS.pop(int(req["id"]), None)
        return None
    raise ValueError(f"unknown op: {op}")

for raw in sys.stdin:
    raw = raw.strip()
    if not raw:
        continue
    try:
        req = json.loads(raw)
        resp = {"ok": True, "val": _dispatch(req)}
    except Exception as ex:
        resp = {"ok": False, "err": f"{type(ex).__name__}: {ex}"}
    _PROTO_OUT.write(json.dumps(resp) + "\n")
    _PROTO_OUT.flush()
"#;

struct PyWorker {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

static PY_BIN: OnceLock<Option<(&'static str, &'static [&'static str])>> = OnceLock::new();
static PY_WORKER: OnceLock<Mutex<Option<PyWorker>>> = OnceLock::new();

fn detect_python() -> Option<(&'static str, &'static [&'static str])> {
    *PY_BIN.get_or_init(|| {
        let candidates: [(&'static str, &'static [&'static str]); 3] =
            [("python", &[]), ("python3", &[]), ("py", &["-3"])];
        for (exe, prefix) in candidates {
            if let Ok(out) = Command::new(exe).args(prefix).arg("--version").output() {
                if out.status.success() {
                    return Some((exe, prefix));
                }
            }
        }
        None
    })
}

fn spawn_worker() -> Result<PyWorker, String> {
    let (exe, prefix) = detect_python()
        .ok_or("no Python 3 interpreter found on PATH (tried python, python3, py)")?;
    let mut cmd = Command::new(exe);
    cmd.args(prefix)
        .arg("-u")
        .arg("-c")
        .arg(WORKER_SCRIPT)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to launch Python worker ({exe}): {e}"))?;
    let stdin = child.stdin.take().ok_or("missing Python stdin pipe")?;
    let stdout = BufReader::new(child.stdout.take().ok_or("missing Python stdout pipe")?);
    Ok(PyWorker {
        child,
        stdin,
        stdout,
    })
}

impl PyWorker {
    fn request(&mut self, payload: &JsonValue) -> Result<JsonValue, String> {
        let line = serde_json::to_string(payload).map_err(|e| e.to_string())?;
        self.stdin
            .write_all(line.as_bytes())
            .and_then(|_| self.stdin.write_all(b"\n"))
            .and_then(|_| self.stdin.flush())
            .map_err(|e| format!("python worker write failed: {e}"))?;
        let mut resp_line = String::new();
        let n = self
            .stdout
            .read_line(&mut resp_line)
            .map_err(|e| format!("python worker read failed: {e}"))?;
        if n == 0 {
            return Err("python worker exited unexpectedly".into());
        }
        let resp: JsonValue = serde_json::from_str(resp_line.trim())
            .map_err(|e| format!("python worker malformed JSON: {e}"))?;
        if resp.get("ok").and_then(JsonValue::as_bool) == Some(true) {
            Ok(resp.get("val").cloned().unwrap_or(JsonValue::Null))
        } else {
            let msg = resp
                .get("err")
                .and_then(JsonValue::as_str)
                .unwrap_or("python error");
            Err(format!("python: {msg}"))
        }
    }
}

fn send_request(payload: JsonValue) -> Result<JsonValue, String> {
    let lock = PY_WORKER.get_or_init(|| Mutex::new(None));
    let mut guard = lock.lock().map_err(|_| "python bridge mutex poisoned")?;
    if guard.is_none() {
        *guard = Some(spawn_worker()?);
    }
    let res = {
        let worker = guard.as_mut().unwrap();
        worker.request(&payload)
    };
    match res {
        Ok(v) => Ok(v),
        Err(err)
            if err.starts_with("python worker write failed")
                || err.starts_with("python worker read failed")
                || err == "python worker exited unexpectedly" =>
        {
            if let Some(mut old) = guard.take() {
                let _ = old.child.kill();
                let _ = old.child.wait();
            }
            let mut fresh = spawn_worker()?;
            let retry = fresh.request(&payload);
            *guard = Some(fresh);
            retry
        }
        Err(e) => Err(e),
    }
}

pub(crate) fn py_handle_id(val: &Val) -> Option<u64> {
    let Val::Map(pairs) = val else {
        return None;
    };
    let mut id = None;
    let mut has_type = false;
    for (k, v) in pairs {
        match (k, v) {
            (Val::Str(ks), Val::Num(n)) if ks == "__py_handle__" && n.is_finite() => {
                id = Some(*n as u64);
            }
            (Val::Str(ks), Val::Str(_)) if ks == "__py_type__" => {
                has_type = true;
            }
            _ => {}
        }
    }
    if has_type { id } else { None }
}

pub(crate) fn is_py_handle(val: &Val) -> bool {
    py_handle_id(val).is_some()
}

pub(crate) fn val_to_wire(val: &Val) -> JsonValue {
    if let Some(id) = py_handle_id(val) {
        return json!({ "__py_ref__": id });
    }
    match val {
        Val::Null => JsonValue::Null,
        Val::Bool(b) => JsonValue::Bool(*b),
        Val::Str(s) => JsonValue::String(s.clone()),
        Val::Num(n) => {
            if n.is_finite()
                && n.fract() == 0.0
                && *n >= (i64::MIN as f64)
                && *n <= (i64::MAX as f64)
            {
                json!(*n as i64)
            } else {
                json!(*n)
            }
        }
        Val::List(xs) => JsonValue::Array(xs.iter().map(val_to_wire).collect()),
        Val::Map(pairs) => {
            let mut obj = serde_json::Map::new();
            for (k, v) in pairs {
                obj.insert(k.show(), val_to_wire(v));
            }
            JsonValue::Object(obj)
        }
        Val::Object(id) => JsonValue::String(format!("<object:{id}>")),
        Val::Function(name) => JsonValue::String(format!("<function:{name}>")),
        Val::Generator(id) => JsonValue::String(format!("<generator:{id}>")),
    }
}

pub(crate) fn wire_to_val(v: &JsonValue) -> Val {
    match v {
        JsonValue::Object(map)
            if map.len() == 2
                && map.contains_key("__py_handle__")
                && map.contains_key("__py_type__") =>
        {
            Val::Map(vec![
                (
                    Val::Str("__py_handle__".into()),
                    Val::Num(map["__py_handle__"].as_f64().unwrap_or(0.0)),
                ),
                (
                    Val::Str("__py_type__".into()),
                    Val::Str(map["__py_type__"].as_str().unwrap_or("object").to_string()),
                ),
            ])
        }
        JsonValue::Array(xs) => Val::List(xs.iter().map(wire_to_val).collect()),
        JsonValue::Object(obj) => Val::Map(
            obj.iter()
                .map(|(k, val)| (Val::Str(k.clone()), wire_to_val(val)))
                .collect(),
        ),
        other => json_to_val(other),
    }
}

pub(crate) fn py_available() -> bool {
    detect_python().is_some()
}

pub(crate) fn py_import(name: &str) -> Result<Val, String> {
    let raw = send_request(json!({ "op": "import", "name": name }))?;
    Ok(wire_to_val(&raw))
}

pub(crate) fn py_call(callable: &Val, args: &[Val], kwargs: Option<&Val>) -> Result<Val, String> {
    let fn_wire = val_to_wire(callable);
    let args_wire: Vec<JsonValue> = args.iter().map(val_to_wire).collect();
    let kwargs_wire = kwargs.map(val_to_wire).unwrap_or_else(|| json!({}));
    let raw = send_request(json!({
        "op": "call",
        "fn": fn_wire,
        "args": args_wire,
        "kwargs": kwargs_wire
    }))?;
    Ok(wire_to_val(&raw))
}

pub(crate) fn py_call_method(
    obj: &Val,
    method: &str,
    args: &[Val],
    kwargs: Option<&Val>,
) -> Result<Val, String> {
    let args_wire: Vec<JsonValue> = args.iter().map(val_to_wire).collect();
    let kwargs_wire = kwargs.map(val_to_wire).unwrap_or_else(|| json!({}));
    let raw = send_request(json!({
        "op": "call_method",
        "obj": val_to_wire(obj),
        "method": method,
        "args": args_wire,
        "kwargs": kwargs_wire
    }))?;
    Ok(wire_to_val(&raw))
}

pub(crate) fn py_get(obj: &Val, attr: &str) -> Result<Val, String> {
    let raw = send_request(json!({
        "op": "get",
        "obj": val_to_wire(obj),
        "attr": attr
    }))?;
    Ok(wire_to_val(&raw))
}

pub(crate) fn py_set(obj: &Val, attr: &str, val: &Val) -> Result<Val, String> {
    let raw = send_request(json!({
        "op": "set",
        "obj": val_to_wire(obj),
        "attr": attr,
        "val": val_to_wire(val)
    }))?;
    Ok(wire_to_val(&raw))
}

pub(crate) fn py_hasattr(obj: &Val, attr: &str) -> Result<bool, String> {
    let raw = send_request(json!({
        "op": "hasattr",
        "obj": val_to_wire(obj),
        "attr": attr
    }))?;
    Ok(raw.as_bool().unwrap_or(false))
}

pub(crate) fn py_getitem(obj: &Val, key: &Val) -> Result<Val, String> {
    let raw = send_request(json!({
        "op": "getitem",
        "obj": val_to_wire(obj),
        "key": val_to_wire(key)
    }))?;
    Ok(wire_to_val(&raw))
}

pub(crate) fn py_setitem(obj: &Val, key: &Val, val: &Val) -> Result<Val, String> {
    let raw = send_request(json!({
        "op": "setitem",
        "obj": val_to_wire(obj),
        "key": val_to_wire(key),
        "val": val_to_wire(val)
    }))?;
    Ok(wire_to_val(&raw))
}

pub(crate) fn py_dir(obj: &Val) -> Result<Val, String> {
    let raw = send_request(json!({
        "op": "dir",
        "obj": val_to_wire(obj)
    }))?;
    Ok(wire_to_val(&raw))
}

pub(crate) fn py_str(obj: &Val) -> Result<String, String> {
    let raw = send_request(json!({
        "op": "str",
        "obj": val_to_wire(obj)
    }))?;
    Ok(raw.as_str().unwrap_or("").to_string())
}

pub(crate) fn py_repr(obj: &Val) -> Result<String, String> {
    let raw = send_request(json!({
        "op": "repr",
        "obj": val_to_wire(obj)
    }))?;
    Ok(raw.as_str().unwrap_or("").to_string())
}

pub(crate) fn py_eval(expr: &str, locals: Option<&Val>) -> Result<Val, String> {
    let loc_wire = locals.map(val_to_wire).unwrap_or_else(|| json!({}));
    let raw = send_request(json!({
        "op": "eval",
        "expr": expr,
        "locals": loc_wire
    }))?;
    Ok(wire_to_val(&raw))
}

pub(crate) fn py_exec(code: &str) -> Result<Val, String> {
    let _ = send_request(json!({
        "op": "exec",
        "code": code
    }))?;
    Ok(Val::Null)
}
