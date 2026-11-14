// Unified Python 3 runtime bridge for Veyra's Horizon engine:
// 1. In-process CPython C-API hosting via `libloading` (`python3XX.dll` / `libpython3*.so`)
//    with GIL management and PEP 3118 `Py_buffer` zero-copy array <-> Tensor exchange.
// 2. Persistent out-of-process JSON-RPC stdio worker (`PyWorker`) fallback when no shared
//    libpython library is present or `VEYRA_PYTHON_BRIDGE=subprocess` is requested.

use std::{
    cell::RefCell,
    collections::HashMap,
    env,
    ffi::{CStr, CString, c_void},
    io::{BufRead, BufReader, Write},
    os::raw::{c_char, c_int, c_long},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, AtomicUsize, Ordering},
        mpsc,
    },
};

use libloading::Library;
use serde_json::{Value as JsonValue, json};

use crate::tensor::{TensorOp, tensor_id, tensor_snapshot, tensor_store};
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
    if op == "tensor_export":
        obj = _from_wire(req["obj"])
        if hasattr(obj, "detach"):
            obj = obj.detach().cpu().numpy()
        shape = list(getattr(obj, "shape", []))
        raw = obj.tolist()
        flat = []
        def _flat(x):
            if isinstance(x, list):
                for e in x:
                    _flat(e)
            else:
                flat.append(float(x))
        _flat(raw)
        if not shape:
            shape = [len(flat)] if flat else [1]
        return {"shape": shape, "data": flat}
    if op == "tensor_import":
        shape = req["shape"]
        data = req["data"]
        try:
            np = importlib.import_module("numpy")
            return _to_wire(np.array(data, dtype="float64").reshape(shape))
        except Exception:
            def _nest(flat_vals, dims):
                if len(dims) <= 1:
                    return list(flat_vals)
                step = 1
                for d in dims[1:]:
                    step *= int(d)
                return [_nest(flat_vals[i:i+step], dims[1:]) for i in range(0, len(flat_vals), step)]
            return _to_wire(_nest(data, shape))
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

// ---------------------------------------------------------------------------
// In-process CPython C-API host (`libloading`)
// ---------------------------------------------------------------------------

#[repr(C)]
struct PyObject {
    _unused: [u8; 0],
}

#[repr(C)]
struct PyBuffer {
    buf: *mut c_void,
    obj: *mut PyObject,
    len: isize,
    itemsize: isize,
    readonly: c_int,
    ndim: c_int,
    format: *mut c_char,
    shape: *mut isize,
    strides: *mut isize,
    suboffsets: *mut isize,
    internal: *mut c_void,
}

const PY_FILE_INPUT: c_int = 257;
const PY_EVAL_INPUT: c_int = 258;
const PYBUF_FULL_RO: c_int = 0x011C;

type PyHostJob = Box<dyn FnOnce(&PyCapiInner) + Send>;

struct PyCapi {
    dll_path: String,
    version: String,
    handle_count: Arc<AtomicUsize>,
    client: Mutex<mpsc::SyncSender<PyHostJob>>,
}

struct PyCapiInner {
    _lib: Library,
    dll_path: String,
    version: String,
    py_inc_ref: unsafe extern "C" fn(*mut PyObject),
    py_dec_ref: unsafe extern "C" fn(*mut PyObject),
    gil_ensure: unsafe extern "C" fn() -> c_int,
    gil_release: unsafe extern "C" fn(c_int),
    import_module: unsafe extern "C" fn(*const c_char) -> *mut PyObject,
    eval_get_builtins: unsafe extern "C" fn() -> *mut PyObject,
    getattr_str: unsafe extern "C" fn(*mut PyObject, *const c_char) -> *mut PyObject,
    setattr_str: unsafe extern "C" fn(*mut PyObject, *const c_char, *mut PyObject) -> c_int,
    hasattr_str: unsafe extern "C" fn(*mut PyObject, *const c_char) -> c_int,
    getitem: unsafe extern "C" fn(*mut PyObject, *mut PyObject) -> *mut PyObject,
    setitem: unsafe extern "C" fn(*mut PyObject, *mut PyObject, *mut PyObject) -> c_int,
    dir: unsafe extern "C" fn(*mut PyObject) -> *mut PyObject,
    obj_str: unsafe extern "C" fn(*mut PyObject) -> *mut PyObject,
    obj_repr: unsafe extern "C" fn(*mut PyObject) -> *mut PyObject,
    obj_type: unsafe extern "C" fn(*mut PyObject) -> *mut PyObject,
    obj_call: unsafe extern "C" fn(*mut PyObject, *mut PyObject, *mut PyObject) -> *mut PyObject,
    obj_isinstance: unsafe extern "C" fn(*mut PyObject, *mut PyObject) -> c_int,
    obj_istrue: unsafe extern "C" fn(*mut PyObject) -> c_int,
    obj_check_buffer: unsafe extern "C" fn(*mut PyObject) -> c_int,
    obj_get_buffer: unsafe extern "C" fn(*mut PyObject, *mut PyBuffer, c_int) -> c_int,
    buffer_is_contiguous: unsafe extern "C" fn(*const PyBuffer, c_char) -> c_int,
    buffer_to_contiguous:
        unsafe extern "C" fn(*mut c_void, *const PyBuffer, isize, c_char) -> c_int,
    buffer_release: unsafe extern "C" fn(*mut PyBuffer),
    tuple_new: unsafe extern "C" fn(isize) -> *mut PyObject,
    tuple_set_item: unsafe extern "C" fn(*mut PyObject, isize, *mut PyObject) -> c_int,
    list_new: unsafe extern "C" fn(isize) -> *mut PyObject,
    list_size: unsafe extern "C" fn(*mut PyObject) -> isize,
    list_get_item: unsafe extern "C" fn(*mut PyObject, isize) -> *mut PyObject,
    list_set_item: unsafe extern "C" fn(*mut PyObject, isize, *mut PyObject) -> c_int,
    dict_new: unsafe extern "C" fn() -> *mut PyObject,
    dict_set_item: unsafe extern "C" fn(*mut PyObject, *mut PyObject, *mut PyObject) -> c_int,
    dict_keys: unsafe extern "C" fn(*mut PyObject) -> *mut PyObject,
    dict_get_item: unsafe extern "C" fn(*mut PyObject, *mut PyObject) -> *mut PyObject,
    unicode_from_str: unsafe extern "C" fn(*const c_char) -> *mut PyObject,
    unicode_as_utf8: unsafe extern "C" fn(*mut PyObject) -> *const c_char,
    long_from_i64: unsafe extern "C" fn(i64) -> *mut PyObject,
    long_as_i64: unsafe extern "C" fn(*mut PyObject) -> i64,
    float_from_f64: unsafe extern "C" fn(f64) -> *mut PyObject,
    float_as_f64: unsafe extern "C" fn(*mut PyObject) -> f64,
    bool_from_long: unsafe extern "C" fn(c_long) -> *mut PyObject,
    bytes_from_slice: unsafe extern "C" fn(*const c_char, isize) -> *mut PyObject,
    err_occurred: unsafe extern "C" fn() -> *mut PyObject,
    err_fetch: unsafe extern "C" fn(*mut *mut PyObject, *mut *mut PyObject, *mut *mut PyObject),
    err_normalize: unsafe extern "C" fn(*mut *mut PyObject, *mut *mut PyObject, *mut *mut PyObject),
    err_clear: unsafe extern "C" fn(),
    compile_string: unsafe extern "C" fn(*const c_char, *const c_char, c_int) -> *mut PyObject,
    eval_code: unsafe extern "C" fn(*mut PyObject, *mut PyObject, *mut PyObject) -> *mut PyObject,
    none_ptr: *mut PyObject,
    true_ptr: *mut PyObject,
    false_ptr: *mut PyObject,
    type_unicode: *mut PyObject,
    type_long: *mut PyObject,
    type_float: *mut PyObject,
    type_bool: *mut PyObject,
    type_list: *mut PyObject,
    type_tuple: *mut PyObject,
    type_dict: *mut PyObject,
    env_dict: *mut PyObject,
    next_id: AtomicU64,
    handles: RefCell<HashMap<u64, *mut PyObject>>,
    handle_count: Arc<AtomicUsize>,
}

struct GilGuard<'a> {
    api: &'a PyCapiInner,
    state: c_int,
}

impl<'a> GilGuard<'a> {
    fn acquire(api: &'a PyCapiInner) -> Self {
        let state = unsafe { (api.gil_ensure)() };
        Self { api, state }
    }
}

impl Drop for GilGuard<'_> {
    fn drop(&mut self) {
        unsafe { (self.api.gil_release)(self.state) };
    }
}

unsafe fn load_fn<T: Copy>(lib: &Library, sym: &[u8]) -> Result<T, String> {
    let s: libloading::Symbol<T> = unsafe { lib.get(sym) }.map_err(|e| {
        format!(
            "missing Python C-API symbol {}: {e}",
            String::from_utf8_lossy(sym)
        )
    })?;
    Ok(*s)
}

unsafe fn load_data_ptr(lib: &Library, sym: &[u8]) -> Result<*mut PyObject, String> {
    let s: libloading::Symbol<*mut PyObject> = unsafe { lib.get(sym) }.map_err(|e| {
        format!(
            "missing Python C-API data {}: {e}",
            String::from_utf8_lossy(sym)
        )
    })?;
    Ok(*s)
}

fn discover_python_dll() -> Option<(PathBuf, Vec<PathBuf>)> {
    let mut dll_candidates: Vec<PathBuf> = Vec::new();
    let mut site_paths: Vec<PathBuf> = Vec::new();

    if let Ok(explicit) = env::var("VEYRA_PYTHON_DLL") {
        let p = PathBuf::from(explicit);
        if p.is_file() {
            dll_candidates.push(p);
        }
    }

    if let Ok(cwd) = env::current_dir() {
        for sub in [
            ".venv/Lib/site-packages",
            "venv/Lib/site-packages",
            ".venv/lib",
        ] {
            let candidate = cwd.join(sub);
            if candidate.is_dir() {
                site_paths.push(candidate);
            }
        }
    }

    if let Some((exe, prefix)) = detect_python() {
        let probe = "import os, sys, sysconfig; print(sys.executable); print(sys.base_prefix); print(sysconfig.get_config_var('LIBDIR') or ''); print(sysconfig.get_config_var('LDLIBRARY') or ''); print(f'{sys.version_info.major}.{sys.version_info.minor}')";
        if let Ok(out) = Command::new(exe).args(prefix).arg("-c").arg(probe).output() {
            if out.status.success() {
                let text = String::from_utf8_lossy(&out.stdout);
                let lines: Vec<&str> = text.lines().map(str::trim).collect();
                if lines.len() >= 5 {
                    let exe_dir = Path::new(lines[0]).parent().map(Path::to_path_buf);
                    let base_prefix = PathBuf::from(lines[1]);
                    let libdir = PathBuf::from(lines[2]);
                    let ldlib = lines[3];
                    let majmin = lines[4];
                    let nodot = majmin.replace('.', "");
                    if cfg!(windows) {
                        for base in [
                            exe_dir.as_deref(),
                            Some(base_prefix.as_path()),
                            Some(libdir.as_path()),
                        ]
                        .into_iter()
                        .flatten()
                        {
                            let p = base.join(format!("python{nodot}.dll"));
                            if p.is_file() {
                                dll_candidates.push(p);
                            }
                        }
                    } else {
                        if !ldlib.is_empty() {
                            if !libdir.as_os_str().is_empty() {
                                let p = libdir.join(ldlib);
                                if p.is_file() {
                                    dll_candidates.push(p);
                                }
                            }
                            let p2 = PathBuf::from(ldlib);
                            if p2.is_file() {
                                dll_candidates.push(p2);
                            }
                        }
                        let ext = if cfg!(target_os = "macos") {
                            "dylib"
                        } else {
                            "so"
                        };
                        let named = format!("libpython{majmin}.{ext}");
                        for dir in [libdir.as_path(), &base_prefix.join("lib")] {
                            let p = dir.join(&named);
                            if p.is_file() {
                                dll_candidates.push(p);
                            }
                        }
                        if cfg!(target_os = "linux") {
                            dll_candidates.push(PathBuf::from(&named));
                        }
                    }
                    let base_site = if cfg!(windows) {
                        base_prefix.join("Lib").join("site-packages")
                    } else {
                        base_prefix
                            .join("lib")
                            .join(format!("python{majmin}"))
                            .join("site-packages")
                    };
                    if base_site.is_dir() {
                        site_paths.push(base_site);
                    }
                }
            }
        }
    }

    dll_candidates
        .into_iter()
        .next()
        .map(|dll| (dll, site_paths))
}

static PY_CAPI: OnceLock<Result<PyCapi, String>> = OnceLock::new();

fn inproc_python_result() -> &'static Result<PyCapi, String> {
    PY_CAPI.get_or_init(PyCapi::try_init)
}

fn inproc_python() -> Option<&'static PyCapi> {
    if matches!(
        env::var("VEYRA_PYTHON_BRIDGE").ok().as_deref(),
        Some("subprocess")
    ) {
        return None;
    }
    inproc_python_result().as_ref().ok()
}

impl PyCapi {
    /// Initializes CPython once on a persistent host thread so `Py_InitializeEx` and all
    /// `PyThreadState` / C-extension TLS state stay bound to a live thread across arbitrary
    /// Veyra worker threads.
    fn try_init() -> Result<Self, String> {
        let (tx, rx) = mpsc::sync_channel::<PyHostJob>(0);
        let (init_tx, init_rx) =
            mpsc::sync_channel::<Result<(String, String, Arc<AtomicUsize>), String>>(1);
        std::thread::Builder::new()
            .name("veyra-cpython-host".to_string())
            .spawn(move || match PyCapiInner::try_init() {
                Ok(inner) => {
                    let meta = (
                        inner.dll_path.clone(),
                        inner.version.clone(),
                        Arc::clone(&inner.handle_count),
                    );
                    let _ = init_tx.send(Ok(meta));
                    while let Ok(job) = rx.recv() {
                        job(&inner);
                    }
                }
                Err(err) => {
                    let _ = init_tx.send(Err(err));
                }
            })
            .map_err(|e| format!("failed to spawn CPython host thread: {e}"))?;
        let (dll_path, version, handle_count) = init_rx
            .recv()
            .map_err(|_| "CPython host thread terminated during initialization".to_string())??;
        Ok(Self {
            dll_path,
            version,
            handle_count,
            client: Mutex::new(tx),
        })
    }

    fn run<R: Send + 'static>(
        &self,
        f: impl FnOnce(&PyCapiInner) -> Result<R, String> + Send + 'static,
    ) -> Result<R, String> {
        let (res_tx, res_rx) = mpsc::sync_channel::<Result<R, String>>(1);
        let job: PyHostJob = Box::new(move |inner| {
            let _ = res_tx.send(f(inner));
        });
        {
            let tx = self
                .client
                .lock()
                .map_err(|_| "CPython host lock poisoned".to_string())?;
            tx.send(job)
                .map_err(|_| "CPython host thread exited unexpectedly".to_string())?;
        }
        res_rx
            .recv()
            .map_err(|_| "CPython host thread failed to respond".to_string())?
    }
}

impl PyCapiInner {
    fn try_init() -> Result<Self, String> {
        let (dll_path, site_paths) = discover_python_dll().ok_or("shared libpython not found")?;
        let lib = unsafe { Library::new(&dll_path) }
            .map_err(|e| format!("failed to load {}: {e}", dll_path.display()))?;

        let init_ex: unsafe extern "C" fn(c_int) = unsafe { load_fn(&lib, b"Py_InitializeEx\0")? };
        let is_init: unsafe extern "C" fn() -> c_int =
            unsafe { load_fn(&lib, b"Py_IsInitialized\0")? };
        let py_inc_ref = unsafe { load_fn(&lib, b"Py_IncRef\0")? };
        let py_dec_ref: unsafe extern "C" fn(*mut PyObject) =
            unsafe { load_fn(&lib, b"Py_DecRef\0")? };
        let gil_ensure: unsafe extern "C" fn() -> c_int =
            unsafe { load_fn(&lib, b"PyGILState_Ensure\0")? };
        let gil_release: unsafe extern "C" fn(c_int) =
            unsafe { load_fn(&lib, b"PyGILState_Release\0")? };
        let import_module: unsafe extern "C" fn(*const c_char) -> *mut PyObject =
            unsafe { load_fn(&lib, b"PyImport_ImportModule\0")? };
        let eval_get_builtins: unsafe extern "C" fn() -> *mut PyObject =
            unsafe { load_fn(&lib, b"PyEval_GetBuiltins\0")? };
        let getattr_str: unsafe extern "C" fn(*mut PyObject, *const c_char) -> *mut PyObject =
            unsafe { load_fn(&lib, b"PyObject_GetAttrString\0")? };
        let setattr_str = unsafe { load_fn(&lib, b"PyObject_SetAttrString\0")? };
        let hasattr_str = unsafe { load_fn(&lib, b"PyObject_HasAttrString\0")? };
        let getitem = unsafe { load_fn(&lib, b"PyObject_GetItem\0")? };
        let setitem = unsafe { load_fn(&lib, b"PyObject_SetItem\0")? };
        let dir = unsafe { load_fn(&lib, b"PyObject_Dir\0")? };
        let obj_str = unsafe { load_fn(&lib, b"PyObject_Str\0")? };
        let obj_repr = unsafe { load_fn(&lib, b"PyObject_Repr\0")? };
        let obj_type = unsafe { load_fn(&lib, b"PyObject_Type\0")? };
        let obj_call: unsafe extern "C" fn(
            *mut PyObject,
            *mut PyObject,
            *mut PyObject,
        ) -> *mut PyObject = unsafe { load_fn(&lib, b"PyObject_Call\0")? };
        let obj_isinstance = unsafe { load_fn(&lib, b"PyObject_IsInstance\0")? };
        let obj_istrue = unsafe { load_fn(&lib, b"PyObject_IsTrue\0")? };
        let obj_check_buffer = unsafe { load_fn(&lib, b"PyObject_CheckBuffer\0")? };
        let obj_get_buffer = unsafe { load_fn(&lib, b"PyObject_GetBuffer\0")? };
        let buffer_is_contiguous = unsafe { load_fn(&lib, b"PyBuffer_IsContiguous\0")? };
        let buffer_to_contiguous = unsafe { load_fn(&lib, b"PyBuffer_ToContiguous\0")? };
        let buffer_release = unsafe { load_fn(&lib, b"PyBuffer_Release\0")? };
        let tuple_new: unsafe extern "C" fn(isize) -> *mut PyObject =
            unsafe { load_fn(&lib, b"PyTuple_New\0")? };
        let tuple_set_item: unsafe extern "C" fn(*mut PyObject, isize, *mut PyObject) -> c_int =
            unsafe { load_fn(&lib, b"PyTuple_SetItem\0")? };
        let list_new = unsafe { load_fn(&lib, b"PyList_New\0")? };
        let list_size = unsafe { load_fn(&lib, b"PyList_Size\0")? };
        let list_get_item = unsafe { load_fn(&lib, b"PyList_GetItem\0")? };
        let list_set_item = unsafe { load_fn(&lib, b"PyList_SetItem\0")? };
        let dict_new: unsafe extern "C" fn() -> *mut PyObject =
            unsafe { load_fn(&lib, b"PyDict_New\0")? };
        let dict_set_item = unsafe { load_fn(&lib, b"PyDict_SetItem\0")? };
        let dict_set_item_str: unsafe extern "C" fn(
            *mut PyObject,
            *const c_char,
            *mut PyObject,
        ) -> c_int = unsafe { load_fn(&lib, b"PyDict_SetItemString\0")? };
        let dict_keys = unsafe { load_fn(&lib, b"PyDict_Keys\0")? };
        let dict_get_item = unsafe { load_fn(&lib, b"PyDict_GetItem\0")? };
        let unicode_from_str: unsafe extern "C" fn(*const c_char) -> *mut PyObject =
            unsafe { load_fn(&lib, b"PyUnicode_FromString\0")? };
        let unicode_as_utf8: unsafe extern "C" fn(*mut PyObject) -> *const c_char =
            unsafe { load_fn(&lib, b"PyUnicode_AsUTF8\0")? };
        let long_from_i64: unsafe extern "C" fn(i64) -> *mut PyObject =
            unsafe { load_fn(&lib, b"PyLong_FromLongLong\0")? };
        let long_as_i64 = unsafe { load_fn(&lib, b"PyLong_AsLongLong\0")? };
        let float_from_f64 = unsafe { load_fn(&lib, b"PyFloat_FromDouble\0")? };
        let float_as_f64 = unsafe { load_fn(&lib, b"PyFloat_AsDouble\0")? };
        let bool_from_long = unsafe { load_fn(&lib, b"PyBool_FromLong\0")? };
        let bytes_from_slice = unsafe { load_fn(&lib, b"PyBytes_FromStringAndSize\0")? };
        let err_occurred = unsafe { load_fn(&lib, b"PyErr_Occurred\0")? };
        let err_fetch = unsafe { load_fn(&lib, b"PyErr_Fetch\0")? };
        let err_normalize = unsafe { load_fn(&lib, b"PyErr_NormalizeException\0")? };
        let err_clear: unsafe extern "C" fn() = unsafe { load_fn(&lib, b"PyErr_Clear\0")? };
        let compile_string = unsafe { load_fn(&lib, b"Py_CompileString\0")? };
        let eval_code = unsafe { load_fn(&lib, b"PyEval_EvalCode\0")? };
        let eval_save_thread: unsafe extern "C" fn() -> *mut c_void =
            unsafe { load_fn(&lib, b"PyEval_SaveThread\0")? };

        let none_ptr = unsafe { load_data_ptr(&lib, b"_Py_NoneStruct\0")? };
        let true_ptr = unsafe { load_data_ptr(&lib, b"_Py_TrueStruct\0")? };
        let false_ptr = unsafe { load_data_ptr(&lib, b"_Py_FalseStruct\0")? };
        let type_unicode = unsafe { load_data_ptr(&lib, b"PyUnicode_Type\0")? };
        let type_long = unsafe { load_data_ptr(&lib, b"PyLong_Type\0")? };
        let type_float = unsafe { load_data_ptr(&lib, b"PyFloat_Type\0")? };
        let type_bool = unsafe { load_data_ptr(&lib, b"PyBool_Type\0")? };
        let type_list = unsafe { load_data_ptr(&lib, b"PyList_Type\0")? };
        let type_tuple = unsafe { load_data_ptr(&lib, b"PyTuple_Type\0")? };
        let type_dict = unsafe { load_data_ptr(&lib, b"PyDict_Type\0")? };

        if unsafe { is_init() } == 0 {
            unsafe {
                init_ex(0);
                // Release the initial GIL held by the initializing thread so arbitrary
                // Veyra worker threads (e.g. cargo test threads) can acquire/release it safely.
                let _ = eval_save_thread();
            }
        }

        let gil = unsafe { gil_ensure() };
        let env_dict = unsafe { dict_new() };
        if env_dict.is_null() {
            unsafe { gil_release(gil) };
            return Err("failed to allocate Python eval environment".into());
        }
        let builtins = unsafe { eval_get_builtins() };
        if !builtins.is_null() {
            unsafe {
                dict_set_item_str(env_dict, c"__builtins__".as_ptr(), builtins);
            }
        }
        // Query sys.version and inject discovered site-packages paths
        let mut version = String::new();
        let sys_mod = unsafe { import_module(c"sys".as_ptr()) };
        if !sys_mod.is_null() {
            let ver_obj = unsafe { getattr_str(sys_mod, c"version".as_ptr()) };
            if !ver_obj.is_null() {
                let p = unsafe { unicode_as_utf8(ver_obj) };
                if !p.is_null() {
                    version = unsafe { CStr::from_ptr(p) }
                        .to_string_lossy()
                        .split_whitespace()
                        .next()
                        .unwrap_or("")
                        .to_string();
                }
                unsafe { py_dec_ref(ver_obj) };
            }
            let sys_path = unsafe { getattr_str(sys_mod, c"path".as_ptr()) };
            if !sys_path.is_null() {
                let insert_m = unsafe { getattr_str(sys_path, c"insert".as_ptr()) };
                if !insert_m.is_null() {
                    for sp in site_paths.iter().rev() {
                        if let Ok(c_sp) = CString::new(sp.to_string_lossy().as_ref()) {
                            let s_obj = unsafe { unicode_from_str(c_sp.as_ptr()) };
                            let idx_obj = unsafe { long_from_i64(0) };
                            let tup = unsafe { tuple_new(2) };
                            if !s_obj.is_null() && !idx_obj.is_null() && !tup.is_null() {
                                unsafe {
                                    tuple_set_item(tup, 0, idx_obj);
                                    tuple_set_item(tup, 1, s_obj);
                                    let res = obj_call(insert_m, tup, std::ptr::null_mut());
                                    if !res.is_null() {
                                        py_dec_ref(res);
                                    }
                                    py_dec_ref(tup);
                                }
                            } else {
                                if !s_obj.is_null() {
                                    unsafe { py_dec_ref(s_obj) };
                                }
                                if !idx_obj.is_null() {
                                    unsafe { py_dec_ref(idx_obj) };
                                }
                                if !tup.is_null() {
                                    unsafe { py_dec_ref(tup) };
                                }
                            }
                        }
                    }
                    unsafe { py_dec_ref(insert_m) };
                }
                unsafe { py_dec_ref(sys_path) };
            }
            unsafe { py_dec_ref(sys_mod) };
        }
        unsafe { err_clear() };
        unsafe { gil_release(gil) };

        Ok(Self {
            _lib: lib,
            dll_path: dll_path.to_string_lossy().to_string(),
            version,
            py_inc_ref,
            py_dec_ref,
            gil_ensure,
            gil_release,
            import_module,
            eval_get_builtins,
            getattr_str,
            setattr_str,
            hasattr_str,
            getitem,
            setitem,
            dir,
            obj_str,
            obj_repr,
            obj_type,
            obj_call,
            obj_isinstance,
            obj_istrue,
            obj_check_buffer,
            obj_get_buffer,
            buffer_is_contiguous,
            buffer_to_contiguous,
            buffer_release,
            tuple_new,
            tuple_set_item,
            list_new,
            list_size,
            list_get_item,
            list_set_item,
            dict_new,
            dict_set_item,
            dict_keys,
            dict_get_item,
            unicode_from_str,
            unicode_as_utf8,
            long_from_i64,
            long_as_i64,
            float_from_f64,
            float_as_f64,
            bool_from_long,
            bytes_from_slice,
            err_occurred,
            err_fetch,
            err_normalize,
            err_clear,
            compile_string,
            eval_code,
            none_ptr,
            true_ptr,
            false_ptr,
            type_unicode,
            type_long,
            type_float,
            type_bool,
            type_list,
            type_tuple,
            type_dict,
            env_dict,
            next_id: AtomicU64::new(1),
            handles: RefCell::new(HashMap::new()),
            handle_count: Arc::new(AtomicUsize::new(0)),
        })
    }

    fn take_error(&self) -> String {
        let mut ptype: *mut PyObject = std::ptr::null_mut();
        let mut pvalue: *mut PyObject = std::ptr::null_mut();
        let mut ptb: *mut PyObject = std::ptr::null_mut();
        unsafe {
            (self.err_fetch)(&mut ptype, &mut pvalue, &mut ptb);
            (self.err_normalize)(&mut ptype, &mut pvalue, &mut ptb);
        }
        let type_name = if !ptype.is_null() {
            let n = unsafe { (self.getattr_str)(ptype, c"__name__".as_ptr()) };
            if !n.is_null() {
                let s = self.py_to_rust_string_owned(n);
                unsafe { (self.py_dec_ref)(n) };
                s.unwrap_or_else(|| "Exception".into())
            } else {
                unsafe { (self.err_clear)() };
                "Exception".into()
            }
        } else {
            "PythonError".into()
        };
        let msg = if !pvalue.is_null() {
            self.py_obj_str(pvalue).unwrap_or_default()
        } else {
            String::new()
        };
        unsafe {
            if !ptype.is_null() {
                (self.py_dec_ref)(ptype);
            }
            if !pvalue.is_null() {
                (self.py_dec_ref)(pvalue);
            }
            if !ptb.is_null() {
                (self.py_dec_ref)(ptb);
            }
            (self.err_clear)();
        }
        if msg.is_empty() {
            format!("python: {type_name}")
        } else {
            format!("python: {type_name}: {msg}")
        }
    }

    fn py_to_rust_string_owned(&self, obj: *mut PyObject) -> Option<String> {
        let p = unsafe { (self.unicode_as_utf8)(obj) };
        if p.is_null() {
            unsafe { (self.err_clear)() };
            return None;
        }
        Some(unsafe { CStr::from_ptr(p) }.to_string_lossy().to_string())
    }

    fn py_obj_str(&self, obj: *mut PyObject) -> Result<String, String> {
        let s_obj = unsafe { (self.obj_str)(obj) };
        if s_obj.is_null() {
            return Err(self.take_error());
        }
        let out = self.py_to_rust_string_owned(s_obj).unwrap_or_default();
        unsafe { (self.py_dec_ref)(s_obj) };
        Ok(out)
    }

    fn py_obj_repr(&self, obj: *mut PyObject) -> Result<String, String> {
        let s_obj = unsafe { (self.obj_repr)(obj) };
        if s_obj.is_null() {
            return Err(self.take_error());
        }
        let out = self.py_to_rust_string_owned(s_obj).unwrap_or_default();
        unsafe { (self.py_dec_ref)(s_obj) };
        Ok(out)
    }

    fn py_type_name(&self, obj: *mut PyObject) -> String {
        let ty = unsafe { (self.obj_type)(obj) };
        if ty.is_null() {
            unsafe { (self.err_clear)() };
            return "object".into();
        }
        let n = unsafe { (self.getattr_str)(ty, c"__name__".as_ptr()) };
        unsafe { (self.py_dec_ref)(ty) };
        if n.is_null() {
            unsafe { (self.err_clear)() };
            return "object".into();
        }
        let res = self
            .py_to_rust_string_owned(n)
            .unwrap_or_else(|| "object".into());
        unsafe { (self.py_dec_ref)(n) };
        res
    }

    fn pin_handle(&self, obj: *mut PyObject) -> Result<Val, String> {
        let type_name = self.py_type_name(obj);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        unsafe { (self.py_inc_ref)(obj) };
        let len = {
            let mut guard = self.handles.borrow_mut();
            guard.insert(id, obj);
            guard.len()
        };
        self.handle_count.store(len, Ordering::Relaxed);
        Ok(Val::Map(vec![
            (Val::Str("__py_handle__".into()), Val::Num(id as f64)),
            (Val::Str("__py_type__".into()), Val::Str(type_name)),
        ]))
    }

    fn lookup_handle(&self, id: u64) -> Result<*mut PyObject, String> {
        self.handles
            .borrow()
            .get(&id)
            .copied()
            .ok_or_else(|| format!("python: KeyError: dangling python handle {id}"))
    }

    fn py_to_val(&self, obj: *mut PyObject, depth: usize) -> Result<Val, String> {
        if obj.is_null() {
            return Err(self.take_error());
        }
        if obj == self.none_ptr {
            return Ok(Val::Null);
        }
        if obj == self.true_ptr {
            return Ok(Val::Bool(true));
        }
        if obj == self.false_ptr {
            return Ok(Val::Bool(false));
        }
        if unsafe { (self.obj_isinstance)(obj, self.type_bool) } == 1 {
            return Ok(Val::Bool(unsafe { (self.obj_istrue)(obj) } != 0));
        }
        if unsafe { (self.obj_isinstance)(obj, self.type_long) } == 1 {
            let ty = unsafe { (self.obj_type)(obj) };
            let is_exact_int = ty == self.type_long;
            if !ty.is_null() {
                unsafe { (self.py_dec_ref)(ty) };
            }
            if is_exact_int {
                let n = unsafe { (self.long_as_i64)(obj) };
                if !unsafe { (self.err_occurred)() }.is_null() {
                    unsafe { (self.err_clear)() };
                    return self.pin_handle(obj);
                }
                return Ok(Val::Num(n as f64));
            }
        }
        if unsafe { (self.obj_isinstance)(obj, self.type_float) } == 1 {
            let ty = unsafe { (self.obj_type)(obj) };
            let is_exact_float = ty == self.type_float;
            if !ty.is_null() {
                unsafe { (self.py_dec_ref)(ty) };
            }
            if is_exact_float {
                let x = unsafe { (self.float_as_f64)(obj) };
                if !unsafe { (self.err_occurred)() }.is_null() {
                    unsafe { (self.err_clear)() };
                    return self.pin_handle(obj);
                }
                return Ok(Val::Num(x));
            }
        }
        if unsafe { (self.obj_isinstance)(obj, self.type_unicode) } == 1 {
            let ty = unsafe { (self.obj_type)(obj) };
            let is_exact_str = ty == self.type_unicode;
            if !ty.is_null() {
                unsafe { (self.py_dec_ref)(ty) };
            }
            if is_exact_str {
                if let Some(s) = self.py_to_rust_string_owned(obj) {
                    return Ok(Val::Str(s));
                }
            }
        }
        if depth < 32 {
            let ty = unsafe { (self.obj_type)(obj) };
            let is_exact_list = ty == self.type_list;
            let is_exact_tuple = ty == self.type_tuple;
            let is_exact_dict = ty == self.type_dict;
            if !ty.is_null() {
                unsafe { (self.py_dec_ref)(ty) };
            }
            if is_exact_list {
                let n = unsafe { (self.list_size)(obj) };
                if (0..=4096).contains(&n) {
                    let mut out = Vec::with_capacity(n as usize);
                    for i in 0..n {
                        let item = unsafe { (self.list_get_item)(obj, i) };
                        out.push(self.py_to_val(item, depth + 1)?);
                    }
                    return Ok(Val::List(out));
                }
            } else if is_exact_tuple {
                let n_obj = self.call_builtin_one("len", obj);
                if let Ok(n) = n_obj {
                    if (0..=4096).contains(&n) {
                        let mut out = Vec::with_capacity(n as usize);
                        let mut ok = true;
                        for idx in 0..n {
                            let idx_py = unsafe { (self.long_from_i64)(idx) };
                            let item = unsafe { (self.getitem)(obj, idx_py) };
                            unsafe { (self.py_dec_ref)(idx_py) };
                            if item.is_null() {
                                unsafe { (self.err_clear)() };
                                ok = false;
                                break;
                            }
                            let converted = self.py_to_val(item, depth + 1);
                            unsafe { (self.py_dec_ref)(item) };
                            match converted {
                                Ok(v) => out.push(v),
                                Err(_) => {
                                    ok = false;
                                    break;
                                }
                            }
                        }
                        if ok {
                            return Ok(Val::List(out));
                        }
                    }
                }
                unsafe { (self.err_clear)() };
            } else if is_exact_dict {
                let keys = unsafe { (self.dict_keys)(obj) };
                if !keys.is_null() {
                    let n = unsafe { (self.list_size)(keys) };
                    if (0..=2048).contains(&n) {
                        let mut pairs = Vec::with_capacity(n as usize);
                        let mut has_handle_shape = false;
                        let mut has_handle_k = false;
                        let mut has_type_k = false;
                        for i in 0..n {
                            let k = unsafe { (self.list_get_item)(keys, i) };
                            let v = unsafe { (self.dict_get_item)(obj, k) };
                            let ks = self.py_obj_str(k)?;
                            if ks == "__py_handle__" {
                                has_handle_k = true;
                            } else if ks == "__py_type__" {
                                has_type_k = true;
                            }
                            pairs.push((Val::Str(ks), self.py_to_val(v, depth + 1)?));
                        }
                        if pairs.len() == 2 && has_handle_k && has_type_k {
                            has_handle_shape = true;
                        }
                        unsafe { (self.py_dec_ref)(keys) };
                        if has_handle_shape {
                            return self.pin_handle(obj);
                        }
                        return Ok(Val::Map(pairs));
                    }
                    unsafe { (self.py_dec_ref)(keys) };
                }
            }
        }
        self.pin_handle(obj)
    }

    fn call_builtin_one(&self, name: &str, arg: *mut PyObject) -> Result<i64, String> {
        let builtins = unsafe { (self.eval_get_builtins)() };
        if builtins.is_null() {
            return Err("missing Python builtins".into());
        }
        let cname = CString::new(name).map_err(|e| e.to_string())?;
        let key = unsafe { (self.unicode_from_str)(cname.as_ptr()) };
        let func = unsafe { (self.dict_get_item)(builtins, key) };
        unsafe { (self.py_dec_ref)(key) };
        if func.is_null() {
            return Err(format!("missing builtin {name}"));
        }
        let tup = unsafe { (self.tuple_new)(1) };
        unsafe {
            (self.py_inc_ref)(arg);
            (self.tuple_set_item)(tup, 0, arg);
        }
        let res = unsafe { (self.obj_call)(func, tup, std::ptr::null_mut()) };
        unsafe { (self.py_dec_ref)(tup) };
        if res.is_null() {
            return Err(self.take_error());
        }
        let n = unsafe { (self.long_as_i64)(res) };
        unsafe { (self.py_dec_ref)(res) };
        Ok(n)
    }

    /// Converts a Veyra `Val` to a new owned `*mut PyObject`.
    fn val_to_py(&self, val: &Val) -> Result<*mut PyObject, String> {
        if let Some(id) = py_handle_id(val) {
            let ptr = self.lookup_handle(id)?;
            unsafe { (self.py_inc_ref)(ptr) };
            return Ok(ptr);
        }
        match val {
            Val::Null => {
                unsafe { (self.py_inc_ref)(self.none_ptr) };
                Ok(self.none_ptr)
            }
            Val::Bool(b) => {
                let p = unsafe { (self.bool_from_long)(i32::from(*b) as c_long) };
                if p.is_null() {
                    return Err(self.take_error());
                }
                Ok(p)
            }
            Val::Num(n) => {
                if n.is_finite()
                    && n.fract() == 0.0
                    && *n >= (i64::MIN as f64)
                    && *n <= (i64::MAX as f64)
                {
                    let p = unsafe { (self.long_from_i64)(*n as i64) };
                    if p.is_null() {
                        return Err(self.take_error());
                    }
                    Ok(p)
                } else {
                    let p = unsafe { (self.float_from_f64)(*n) };
                    if p.is_null() {
                        return Err(self.take_error());
                    }
                    Ok(p)
                }
            }
            Val::Str(s) => {
                let cs = CString::new(s.as_str()).map_err(|e| e.to_string())?;
                let p = unsafe { (self.unicode_from_str)(cs.as_ptr()) };
                if p.is_null() {
                    return Err(self.take_error());
                }
                Ok(p)
            }
            Val::List(items) => {
                let list = unsafe { (self.list_new)(items.len() as isize) };
                if list.is_null() {
                    return Err(self.take_error());
                }
                for (idx, item) in items.iter().enumerate() {
                    match self.val_to_py(item) {
                        Ok(py_item) => {
                            if unsafe { (self.list_set_item)(list, idx as isize, py_item) } != 0 {
                                unsafe { (self.py_dec_ref)(list) };
                                return Err(self.take_error());
                            }
                        }
                        Err(e) => {
                            unsafe { (self.py_dec_ref)(list) };
                            return Err(e);
                        }
                    }
                }
                Ok(list)
            }
            Val::Map(pairs) => {
                let dict = unsafe { (self.dict_new)() };
                if dict.is_null() {
                    return Err(self.take_error());
                }
                for (k, v) in pairs {
                    let k_str = k.show();
                    let Ok(ck) = CString::new(k_str.as_str()) else {
                        unsafe { (self.py_dec_ref)(dict) };
                        return Err("dict key contains NUL byte".into());
                    };
                    let pk = unsafe { (self.unicode_from_str)(ck.as_ptr()) };
                    if pk.is_null() {
                        unsafe { (self.py_dec_ref)(dict) };
                        return Err(self.take_error());
                    }
                    let pv = match self.val_to_py(v) {
                        Ok(p) => p,
                        Err(e) => {
                            unsafe {
                                (self.py_dec_ref)(pk);
                                (self.py_dec_ref)(dict);
                            }
                            return Err(e);
                        }
                    };
                    let rc = unsafe { (self.dict_set_item)(dict, pk, pv) };
                    unsafe {
                        (self.py_dec_ref)(pk);
                        (self.py_dec_ref)(pv);
                    }
                    if rc != 0 {
                        unsafe { (self.py_dec_ref)(dict) };
                        return Err(self.take_error());
                    }
                }
                Ok(dict)
            }
            Val::Object(id) => self.val_to_py(&Val::Str(format!("<object:{id}>"))),
            Val::Function(name) => self.val_to_py(&Val::Str(format!("<function:{name}>"))),
            Val::Generator(id) => self.val_to_py(&Val::Str(format!("<generator:{id}>"))),
        }
    }

    fn build_call_args(
        &self,
        args: &[Val],
        kwargs: Option<&Val>,
    ) -> Result<(*mut PyObject, *mut PyObject), String> {
        let tup = unsafe { (self.tuple_new)(args.len() as isize) };
        if tup.is_null() {
            return Err(self.take_error());
        }
        for (idx, a) in args.iter().enumerate() {
            match self.val_to_py(a) {
                Ok(p) => {
                    if unsafe { (self.tuple_set_item)(tup, idx as isize, p) } != 0 {
                        unsafe { (self.py_dec_ref)(tup) };
                        return Err(self.take_error());
                    }
                }
                Err(e) => {
                    unsafe { (self.py_dec_ref)(tup) };
                    return Err(e);
                }
            }
        }
        let kw_ptr = match kwargs {
            None | Some(Val::Null) => std::ptr::null_mut(),
            Some(Val::Map(pairs)) if pairs.is_empty() => std::ptr::null_mut(),
            Some(other) => match self.val_to_py(other) {
                Ok(p) => p,
                Err(e) => {
                    unsafe { (self.py_dec_ref)(tup) };
                    return Err(e);
                }
            },
        };
        Ok((tup, kw_ptr))
    }

    fn import(&self, name: &str) -> Result<Val, String> {
        let _gil = GilGuard::acquire(self);
        let cname = CString::new(name).map_err(|e| e.to_string())?;
        let module = unsafe { (self.import_module)(cname.as_ptr()) };
        if module.is_null() {
            return Err(self.take_error());
        }
        let out = self.py_to_val(module, 0);
        unsafe { (self.py_dec_ref)(module) };
        out
    }

    fn call(&self, callable: &Val, args: &[Val], kwargs: Option<&Val>) -> Result<Val, String> {
        let _gil = GilGuard::acquire(self);
        let func = self.val_to_py(callable)?;
        let (tup, kw) = match self.build_call_args(args, kwargs) {
            Ok(pair) => pair,
            Err(e) => {
                unsafe { (self.py_dec_ref)(func) };
                return Err(e);
            }
        };
        let res = unsafe { (self.obj_call)(func, tup, kw) };
        unsafe {
            (self.py_dec_ref)(func);
            (self.py_dec_ref)(tup);
            if !kw.is_null() {
                (self.py_dec_ref)(kw);
            }
        }
        if res.is_null() {
            return Err(self.take_error());
        }
        let out = self.py_to_val(res, 0);
        unsafe { (self.py_dec_ref)(res) };
        out
    }

    fn call_method(
        &self,
        obj: &Val,
        method: &str,
        args: &[Val],
        kwargs: Option<&Val>,
    ) -> Result<Val, String> {
        let _gil = GilGuard::acquire(self);
        let py_obj = self.val_to_py(obj)?;
        let cm = CString::new(method).map_err(|e| e.to_string())?;
        let func = unsafe { (self.getattr_str)(py_obj, cm.as_ptr()) };
        unsafe { (self.py_dec_ref)(py_obj) };
        if func.is_null() {
            return Err(self.take_error());
        }
        let (tup, kw) = match self.build_call_args(args, kwargs) {
            Ok(pair) => pair,
            Err(e) => {
                unsafe { (self.py_dec_ref)(func) };
                return Err(e);
            }
        };
        let res = unsafe { (self.obj_call)(func, tup, kw) };
        unsafe {
            (self.py_dec_ref)(func);
            (self.py_dec_ref)(tup);
            if !kw.is_null() {
                (self.py_dec_ref)(kw);
            }
        }
        if res.is_null() {
            return Err(self.take_error());
        }
        let out = self.py_to_val(res, 0);
        unsafe { (self.py_dec_ref)(res) };
        out
    }

    fn get(&self, obj: &Val, attr: &str) -> Result<Val, String> {
        let _gil = GilGuard::acquire(self);
        let py_obj = self.val_to_py(obj)?;
        let ca = CString::new(attr).map_err(|e| e.to_string())?;
        let val = unsafe { (self.getattr_str)(py_obj, ca.as_ptr()) };
        unsafe { (self.py_dec_ref)(py_obj) };
        if val.is_null() {
            return Err(self.take_error());
        }
        let out = self.py_to_val(val, 0);
        unsafe { (self.py_dec_ref)(val) };
        out
    }

    fn set(&self, obj: &Val, attr: &str, val: &Val) -> Result<Val, String> {
        let _gil = GilGuard::acquire(self);
        let py_obj = self.val_to_py(obj)?;
        let py_val = match self.val_to_py(val) {
            Ok(v) => v,
            Err(e) => {
                unsafe { (self.py_dec_ref)(py_obj) };
                return Err(e);
            }
        };
        let ca = CString::new(attr).map_err(|e| e.to_string())?;
        let rc = unsafe { (self.setattr_str)(py_obj, ca.as_ptr(), py_val) };
        unsafe { (self.py_dec_ref)(py_obj) };
        if rc != 0 {
            unsafe { (self.py_dec_ref)(py_val) };
            return Err(self.take_error());
        }
        let out = self.py_to_val(py_val, 0);
        unsafe { (self.py_dec_ref)(py_val) };
        out
    }

    fn hasattr(&self, obj: &Val, attr: &str) -> Result<bool, String> {
        let _gil = GilGuard::acquire(self);
        let py_obj = self.val_to_py(obj)?;
        let ca = CString::new(attr).map_err(|e| e.to_string())?;
        let res = unsafe { (self.hasattr_str)(py_obj, ca.as_ptr()) };
        unsafe { (self.py_dec_ref)(py_obj) };
        Ok(res != 0)
    }

    fn getitem(&self, obj: &Val, key: &Val) -> Result<Val, String> {
        let _gil = GilGuard::acquire(self);
        let py_obj = self.val_to_py(obj)?;
        let py_key = match self.val_to_py(key) {
            Ok(k) => k,
            Err(e) => {
                unsafe { (self.py_dec_ref)(py_obj) };
                return Err(e);
            }
        };
        let item = unsafe { (self.getitem)(py_obj, py_key) };
        unsafe {
            (self.py_dec_ref)(py_obj);
            (self.py_dec_ref)(py_key);
        }
        if item.is_null() {
            return Err(self.take_error());
        }
        let out = self.py_to_val(item, 0);
        unsafe { (self.py_dec_ref)(item) };
        out
    }

    fn setitem(&self, obj: &Val, key: &Val, val: &Val) -> Result<Val, String> {
        let _gil = GilGuard::acquire(self);
        let py_obj = self.val_to_py(obj)?;
        let py_key = match self.val_to_py(key) {
            Ok(k) => k,
            Err(e) => {
                unsafe { (self.py_dec_ref)(py_obj) };
                return Err(e);
            }
        };
        let py_val = match self.val_to_py(val) {
            Ok(v) => v,
            Err(e) => {
                unsafe {
                    (self.py_dec_ref)(py_obj);
                    (self.py_dec_ref)(py_key);
                }
                return Err(e);
            }
        };
        let rc = unsafe { (self.setitem)(py_obj, py_key, py_val) };
        unsafe {
            (self.py_dec_ref)(py_obj);
            (self.py_dec_ref)(py_key);
        }
        if rc != 0 {
            unsafe { (self.py_dec_ref)(py_val) };
            return Err(self.take_error());
        }
        let out = self.py_to_val(py_val, 0);
        unsafe { (self.py_dec_ref)(py_val) };
        out
    }

    fn dir_of(&self, obj: &Val) -> Result<Val, String> {
        let _gil = GilGuard::acquire(self);
        let py_obj = self.val_to_py(obj)?;
        let list = unsafe { (self.dir)(py_obj) };
        unsafe { (self.py_dec_ref)(py_obj) };
        if list.is_null() {
            return Err(self.take_error());
        }
        let n = unsafe { (self.list_size)(list) };
        let mut out = Vec::with_capacity(n.max(0) as usize);
        for i in 0..n {
            let item = unsafe { (self.list_get_item)(list, i) };
            if !item.is_null() {
                if let Some(s) = self.py_to_rust_string_owned(item) {
                    out.push(Val::Str(s));
                }
            }
        }
        unsafe { (self.py_dec_ref)(list) };
        Ok(Val::List(out))
    }

    fn str_of(&self, obj: &Val) -> Result<String, String> {
        let _gil = GilGuard::acquire(self);
        let py_obj = self.val_to_py(obj)?;
        let res = self.py_obj_str(py_obj);
        unsafe { (self.py_dec_ref)(py_obj) };
        res
    }

    fn repr_of(&self, obj: &Val) -> Result<String, String> {
        let _gil = GilGuard::acquire(self);
        let py_obj = self.val_to_py(obj)?;
        let res = self.py_obj_repr(py_obj);
        unsafe { (self.py_dec_ref)(py_obj) };
        res
    }

    fn eval_expr(&self, expr: &str, locals: Option<&Val>) -> Result<Val, String> {
        let _gil = GilGuard::acquire(self);
        let cexpr = CString::new(expr).map_err(|e| e.to_string())?;
        let code = unsafe {
            (self.compile_string)(cexpr.as_ptr(), c"<veyra-eval>".as_ptr(), PY_EVAL_INPUT)
        };
        if code.is_null() {
            return Err(self.take_error());
        }
        let loc_ptr = match locals {
            None | Some(Val::Null) => self.env_dict,
            Some(Val::Map(_)) => match self.val_to_py(locals.unwrap()) {
                Ok(p) => p,
                Err(e) => {
                    unsafe { (self.py_dec_ref)(code) };
                    return Err(e);
                }
            },
            Some(_) => {
                unsafe { (self.py_dec_ref)(code) };
                return Err("python: TypeError: py_eval locals must be a dict/Map".into());
            }
        };
        let res = unsafe { (self.eval_code)(code, self.env_dict, loc_ptr) };
        unsafe {
            (self.py_dec_ref)(code);
            if loc_ptr != self.env_dict {
                (self.py_dec_ref)(loc_ptr);
            }
        }
        if res.is_null() {
            return Err(self.take_error());
        }
        let out = self.py_to_val(res, 0);
        unsafe { (self.py_dec_ref)(res) };
        out
    }

    fn exec_code(&self, src: &str) -> Result<Val, String> {
        let _gil = GilGuard::acquire(self);
        let csrc = CString::new(src).map_err(|e| e.to_string())?;
        let code = unsafe {
            (self.compile_string)(csrc.as_ptr(), c"<veyra-exec>".as_ptr(), PY_FILE_INPUT)
        };
        if code.is_null() {
            return Err(self.take_error());
        }
        let env_ptr = self.env_dict;
        let res = unsafe { (self.eval_code)(code, env_ptr, env_ptr) };
        unsafe { (self.py_dec_ref)(code) };
        if res.is_null() {
            return Err(self.take_error());
        }
        unsafe { (self.py_dec_ref)(res) };
        Ok(Val::Null)
    }

    fn free_handle(&self, id: u64) -> Result<bool, String> {
        let _gil = GilGuard::acquire(self);
        let removed = {
            let mut guard = self.handles.borrow_mut();
            let raw = guard.remove(&id);
            (raw, guard.len())
        };
        self.handle_count.store(removed.1, Ordering::Relaxed);
        if let Some(raw) = removed.0 {
            unsafe { (self.py_dec_ref)(raw) };
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn export_to_tensor(&self, obj: &Val) -> Result<Val, String> {
        let _gil = GilGuard::acquire(self);
        let mut py_obj = self.val_to_py(obj)?;
        // Handle PyTorch Tensor cleanly via `.detach().cpu().numpy()` if it lacks direct buffer
        if unsafe { (self.obj_check_buffer)(py_obj) } == 0
            && unsafe { (self.hasattr_str)(py_obj, c"detach".as_ptr()) } != 0
        {
            let det_fn = unsafe { (self.getattr_str)(py_obj, c"detach".as_ptr()) };
            if !det_fn.is_null() {
                let empty = unsafe { (self.tuple_new)(0) };
                let detached = unsafe { (self.obj_call)(det_fn, empty, std::ptr::null_mut()) };
                unsafe {
                    (self.py_dec_ref)(empty);
                    (self.py_dec_ref)(det_fn);
                }
                if !detached.is_null() {
                    let cpu_fn = unsafe { (self.getattr_str)(detached, c"cpu".as_ptr()) };
                    let cpu_obj = if !cpu_fn.is_null() {
                        let empty2 = unsafe { (self.tuple_new)(0) };
                        let r = unsafe { (self.obj_call)(cpu_fn, empty2, std::ptr::null_mut()) };
                        unsafe {
                            (self.py_dec_ref)(empty2);
                            (self.py_dec_ref)(cpu_fn);
                        }
                        if !r.is_null() {
                            unsafe { (self.py_dec_ref)(detached) };
                            r
                        } else {
                            unsafe { (self.err_clear)() };
                            detached
                        }
                    } else {
                        unsafe { (self.err_clear)() };
                        detached
                    };
                    let np_fn = unsafe { (self.getattr_str)(cpu_obj, c"numpy".as_ptr()) };
                    if !np_fn.is_null() {
                        let empty3 = unsafe { (self.tuple_new)(0) };
                        let arr = unsafe { (self.obj_call)(np_fn, empty3, std::ptr::null_mut()) };
                        unsafe {
                            (self.py_dec_ref)(empty3);
                            (self.py_dec_ref)(np_fn);
                        }
                        if !arr.is_null() {
                            unsafe {
                                (self.py_dec_ref)(cpu_obj);
                                (self.py_dec_ref)(py_obj);
                            }
                            py_obj = arr;
                        } else {
                            unsafe {
                                (self.err_clear)();
                                (self.py_dec_ref)(cpu_obj);
                            }
                        }
                    } else {
                        unsafe {
                            (self.err_clear)();
                            (self.py_dec_ref)(cpu_obj);
                        }
                    }
                } else {
                    unsafe { (self.err_clear)() };
                }
            }
        }
        if unsafe { (self.obj_check_buffer)(py_obj) } == 0 {
            unsafe { (self.py_dec_ref)(py_obj) };
            return Err("Python object does not implement PEP 3118 buffer protocol".into());
        }
        let mut view = PyBuffer {
            buf: std::ptr::null_mut(),
            obj: std::ptr::null_mut(),
            len: 0,
            itemsize: 0,
            readonly: 0,
            ndim: 0,
            format: std::ptr::null_mut(),
            shape: std::ptr::null_mut(),
            strides: std::ptr::null_mut(),
            suboffsets: std::ptr::null_mut(),
            internal: std::ptr::null_mut(),
        };
        if unsafe { (self.obj_get_buffer)(py_obj, &mut view, PYBUF_FULL_RO) } != 0 {
            unsafe { (self.py_dec_ref)(py_obj) };
            return Err(self.take_error());
        }

        let ndim = view.ndim.max(0) as usize;
        let shape: Vec<usize> = if ndim == 0 || view.shape.is_null() {
            vec![1]
        } else {
            unsafe { std::slice::from_raw_parts(view.shape, ndim) }
                .iter()
                .map(|&d| d.max(0) as usize)
                .collect()
        };
        let count: usize = shape.iter().product();
        let itemsize = view.itemsize.max(1) as usize;
        let fmt_str = if view.format.is_null() {
            "B".to_string()
        } else {
            unsafe { CStr::from_ptr(view.format) }
                .to_string_lossy()
                .trim_start_matches(['@', '=', '<', '>', '!'])
                .to_string()
        };

        let total_bytes = count.saturating_mul(itemsize);
        let mut scratch: Vec<u8> = Vec::new();
        let raw_bytes: &[u8] = if total_bytes == 0 {
            &[]
        } else if ndim == 0 || unsafe { (self.buffer_is_contiguous)(&view, b'C' as c_char) } != 0 {
            unsafe { std::slice::from_raw_parts(view.buf as *const u8, total_bytes) }
        } else {
            scratch.resize(total_bytes, 0);
            if unsafe {
                (self.buffer_to_contiguous)(
                    scratch.as_mut_ptr() as *mut c_void,
                    &view,
                    total_bytes as isize,
                    b'C' as c_char,
                )
            } != 0
            {
                unsafe {
                    (self.buffer_release)(&mut view);
                    (self.py_dec_ref)(py_obj);
                }
                return Err(self.take_error());
            }
            &scratch
        };

        let mut data = Vec::with_capacity(count);
        for chunk in raw_bytes.chunks_exact(itemsize) {
            let v = decode_buffer_scalar(&fmt_str, itemsize, chunk);
            match v {
                Some(x) => data.push(x),
                None => {
                    unsafe {
                        (self.buffer_release)(&mut view);
                        (self.py_dec_ref)(py_obj);
                    }
                    return Err(format!(
                        "unsupported Python buffer dtype '{fmt_str}' (itemsize={itemsize})"
                    ));
                }
            }
        }
        unsafe {
            (self.buffer_release)(&mut view);
            (self.py_dec_ref)(py_obj);
        }
        tensor_store(data, shape, false, TensorOp::Leaf)
    }

    fn import_from_tensor(&self, tval: &Val) -> Result<Val, String> {
        let id = tensor_id(tval)?;
        let snap = tensor_snapshot(id)?;
        let _gil = GilGuard::acquire(self);

        let raw_ptr = snap.data.as_ptr() as *const c_char;
        let byte_len = (snap.data.len() * std::mem::size_of::<f64>()) as isize;
        let np_mod = unsafe { (self.import_module)(c"numpy".as_ptr()) };
        if !np_mod.is_null() {
            let py_bytes = unsafe { (self.bytes_from_slice)(raw_ptr, byte_len) };
            let frombuffer = unsafe { (self.getattr_str)(np_mod, c"frombuffer".as_ptr()) };
            let dtype_f64 = unsafe { (self.unicode_from_str)(c"float64".as_ptr()) };
            if !py_bytes.is_null() && !frombuffer.is_null() && !dtype_f64.is_null() {
                let tup = unsafe { (self.tuple_new)(2) };
                unsafe {
                    (self.py_inc_ref)(py_bytes);
                    (self.py_inc_ref)(dtype_f64);
                    (self.tuple_set_item)(tup, 0, py_bytes);
                    (self.tuple_set_item)(tup, 1, dtype_f64);
                }
                let arr = unsafe { (self.obj_call)(frombuffer, tup, std::ptr::null_mut()) };
                unsafe {
                    (self.py_dec_ref)(tup);
                    (self.py_dec_ref)(py_bytes);
                    (self.py_dec_ref)(dtype_f64);
                    (self.py_dec_ref)(frombuffer);
                    (self.py_dec_ref)(np_mod);
                }
                if !arr.is_null() {
                    let shape_tup = unsafe { (self.tuple_new)(snap.shape.len() as isize) };
                    for (idx, &dim) in snap.shape.iter().enumerate() {
                        let d_py = unsafe { (self.long_from_i64)(dim as i64) };
                        unsafe { (self.tuple_set_item)(shape_tup, idx as isize, d_py) };
                    }
                    let reshape_m = unsafe { (self.getattr_str)(arr, c"reshape".as_ptr()) };
                    let reshaped = if !reshape_m.is_null() {
                        let wrap = unsafe { (self.tuple_new)(1) };
                        unsafe { (self.tuple_set_item)(wrap, 0, shape_tup) };
                        let r = unsafe { (self.obj_call)(reshape_m, wrap, std::ptr::null_mut()) };
                        unsafe {
                            (self.py_dec_ref)(wrap);
                            (self.py_dec_ref)(reshape_m);
                        }
                        r
                    } else {
                        unsafe { (self.py_dec_ref)(shape_tup) };
                        std::ptr::null_mut()
                    };
                    if !reshaped.is_null() {
                        let copy_m = unsafe { (self.getattr_str)(reshaped, c"copy".as_ptr()) };
                        let copied = if !copy_m.is_null() {
                            let empty = unsafe { (self.tuple_new)(0) };
                            let r = unsafe { (self.obj_call)(copy_m, empty, std::ptr::null_mut()) };
                            unsafe {
                                (self.py_dec_ref)(empty);
                                (self.py_dec_ref)(copy_m);
                            }
                            r
                        } else {
                            std::ptr::null_mut()
                        };
                        unsafe {
                            (self.py_dec_ref)(reshaped);
                            (self.py_dec_ref)(arr);
                        }
                        if !copied.is_null() {
                            let out = self.pin_handle(copied);
                            unsafe { (self.py_dec_ref)(copied) };
                            return out;
                        }
                    } else {
                        unsafe { (self.py_dec_ref)(arr) };
                    }
                }
            } else {
                unsafe {
                    if !py_bytes.is_null() {
                        (self.py_dec_ref)(py_bytes);
                    }
                    if !frombuffer.is_null() {
                        (self.py_dec_ref)(frombuffer);
                    }
                    if !dtype_f64.is_null() {
                        (self.py_dec_ref)(dtype_f64);
                    }
                    (self.py_dec_ref)(np_mod);
                }
            }
            unsafe { (self.err_clear)() };
        } else {
            unsafe { (self.err_clear)() };
        }

        // Pure-Python nested list fallback when numpy isn't installed
        let val = nest_tensor_vals(&snap.data, &snap.shape);
        let py_obj = self.val_to_py(&val)?;
        let out = self.pin_handle(py_obj);
        unsafe { (self.py_dec_ref)(py_obj) };
        out
    }
}

fn decode_buffer_scalar(fmt: &str, itemsize: usize, bytes: &[u8]) -> Option<f64> {
    match (fmt, itemsize) {
        ("d", 8) => Some(f64::from_ne_bytes(bytes.try_into().ok()?)),
        ("f", 4) => Some(f32::from_ne_bytes(bytes.try_into().ok()?) as f64),
        ("q", 8) | ("l", 8) => Some(i64::from_ne_bytes(bytes.try_into().ok()?) as f64),
        ("Q", 8) | ("L", 8) => Some(u64::from_ne_bytes(bytes.try_into().ok()?) as f64),
        ("i", 4) | ("l", 4) => Some(i32::from_ne_bytes(bytes.try_into().ok()?) as f64),
        ("I", 4) | ("L", 4) => Some(u32::from_ne_bytes(bytes.try_into().ok()?) as f64),
        ("h", 2) => Some(i16::from_ne_bytes(bytes.try_into().ok()?) as f64),
        ("H", 2) => Some(u16::from_ne_bytes(bytes.try_into().ok()?) as f64),
        ("b", 1) => Some((bytes[0] as i8) as f64),
        ("B", 1) => Some(bytes[0] as f64),
        ("?", 1) => Some(if bytes[0] != 0 { 1.0 } else { 0.0 }),
        _ => None,
    }
}

fn nest_tensor_vals(data: &[f64], shape: &[usize]) -> Val {
    if shape.len() <= 1 {
        return Val::List(data.iter().copied().map(Val::Num).collect());
    }
    let step: usize = shape[1..].iter().product::<usize>().max(1);
    let mut rows = Vec::new();
    for chunk in data.chunks(step) {
        rows.push(nest_tensor_vals(chunk, &shape[1..]));
    }
    Val::List(rows)
}

// ---------------------------------------------------------------------------
// Out-of-process stdio JSON-RPC worker fallback (`PyWorker`)
// ---------------------------------------------------------------------------

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
    inproc_python().is_some() || detect_python().is_some()
}

pub(crate) fn py_info_val() -> Val {
    if let Some(api) = inproc_python() {
        let count = api.handle_count.load(Ordering::Relaxed);
        Val::Map(vec![
            (Val::Str("available".into()), Val::Bool(true)),
            (Val::Str("mode".into()), Val::Str("in-process-capi".into())),
            (Val::Str("dll".into()), Val::Str(api.dll_path.clone())),
            (Val::Str("version".into()), Val::Str(api.version.clone())),
            (Val::Str("zero_copy_buffer".into()), Val::Bool(true)),
            (Val::Str("handles".into()), Val::Num(count as f64)),
        ])
    } else if detect_python().is_some() {
        let init_err = match inproc_python_result() {
            Err(e) => e.clone(),
            Ok(_) => "disabled by VEYRA_PYTHON_BRIDGE=subprocess".into(),
        };
        Val::Map(vec![
            (Val::Str("available".into()), Val::Bool(true)),
            (
                Val::Str("mode".into()),
                Val::Str("subprocess-jsonrpc".into()),
            ),
            (Val::Str("dll".into()), Val::Str(String::new())),
            (Val::Str("version".into()), Val::Str(String::new())),
            (Val::Str("zero_copy_buffer".into()), Val::Bool(false)),
            (Val::Str("handles".into()), Val::Num(0.0)),
            (Val::Str("init_error".into()), Val::Str(init_err)),
        ])
    } else {
        Val::Map(vec![
            (Val::Str("available".into()), Val::Bool(false)),
            (Val::Str("mode".into()), Val::Str("unavailable".into())),
            (Val::Str("zero_copy_buffer".into()), Val::Bool(false)),
        ])
    }
}

pub(crate) fn py_import(name: &str) -> Result<Val, String> {
    if let Some(api) = inproc_python() {
        let name = name.to_string();
        return api.run(move |c| c.import(&name));
    }
    let raw = send_request(json!({ "op": "import", "name": name }))?;
    Ok(wire_to_val(&raw))
}

pub(crate) fn py_call(callable: &Val, args: &[Val], kwargs: Option<&Val>) -> Result<Val, String> {
    if let Some(api) = inproc_python() {
        let callable = callable.clone();
        let args = args.to_vec();
        let kwargs = kwargs.cloned();
        return api.run(move |c| c.call(&callable, &args, kwargs.as_ref()));
    }
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
    if let Some(api) = inproc_python() {
        let obj = obj.clone();
        let method = method.to_string();
        let args = args.to_vec();
        let kwargs = kwargs.cloned();
        return api.run(move |c| c.call_method(&obj, &method, &args, kwargs.as_ref()));
    }
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
    if let Some(api) = inproc_python() {
        let obj = obj.clone();
        let attr = attr.to_string();
        return api.run(move |c| c.get(&obj, &attr));
    }
    let raw = send_request(json!({
        "op": "get",
        "obj": val_to_wire(obj),
        "attr": attr
    }))?;
    Ok(wire_to_val(&raw))
}

pub(crate) fn py_set(obj: &Val, attr: &str, val: &Val) -> Result<Val, String> {
    if let Some(api) = inproc_python() {
        let obj = obj.clone();
        let attr = attr.to_string();
        let val = val.clone();
        return api.run(move |c| c.set(&obj, &attr, &val));
    }
    let raw = send_request(json!({
        "op": "set",
        "obj": val_to_wire(obj),
        "attr": attr,
        "val": val_to_wire(val)
    }))?;
    Ok(wire_to_val(&raw))
}

pub(crate) fn py_hasattr(obj: &Val, attr: &str) -> Result<bool, String> {
    if let Some(api) = inproc_python() {
        let obj = obj.clone();
        let attr = attr.to_string();
        return api.run(move |c| c.hasattr(&obj, &attr));
    }
    let raw = send_request(json!({
        "op": "hasattr",
        "obj": val_to_wire(obj),
        "attr": attr
    }))?;
    Ok(raw.as_bool().unwrap_or(false))
}

pub(crate) fn py_getitem(obj: &Val, key: &Val) -> Result<Val, String> {
    if let Some(api) = inproc_python() {
        let obj = obj.clone();
        let key = key.clone();
        return api.run(move |c| c.getitem(&obj, &key));
    }
    let raw = send_request(json!({
        "op": "getitem",
        "obj": val_to_wire(obj),
        "key": val_to_wire(key)
    }))?;
    Ok(wire_to_val(&raw))
}

pub(crate) fn py_setitem(obj: &Val, key: &Val, val: &Val) -> Result<Val, String> {
    if let Some(api) = inproc_python() {
        let obj = obj.clone();
        let key = key.clone();
        let val = val.clone();
        return api.run(move |c| c.setitem(&obj, &key, &val));
    }
    let raw = send_request(json!({
        "op": "setitem",
        "obj": val_to_wire(obj),
        "key": val_to_wire(key),
        "val": val_to_wire(val)
    }))?;
    Ok(wire_to_val(&raw))
}

pub(crate) fn py_dir(obj: &Val) -> Result<Val, String> {
    if let Some(api) = inproc_python() {
        let obj = obj.clone();
        return api.run(move |c| c.dir_of(&obj));
    }
    let raw = send_request(json!({
        "op": "dir",
        "obj": val_to_wire(obj)
    }))?;
    Ok(wire_to_val(&raw))
}

pub(crate) fn py_str(obj: &Val) -> Result<String, String> {
    if let Some(api) = inproc_python() {
        let obj = obj.clone();
        return api.run(move |c| c.str_of(&obj));
    }
    let raw = send_request(json!({
        "op": "str",
        "obj": val_to_wire(obj)
    }))?;
    Ok(raw.as_str().unwrap_or("").to_string())
}

pub(crate) fn py_repr(obj: &Val) -> Result<String, String> {
    if let Some(api) = inproc_python() {
        let obj = obj.clone();
        return api.run(move |c| c.repr_of(&obj));
    }
    let raw = send_request(json!({
        "op": "repr",
        "obj": val_to_wire(obj)
    }))?;
    Ok(raw.as_str().unwrap_or("").to_string())
}

pub(crate) fn py_eval(expr: &str, locals: Option<&Val>) -> Result<Val, String> {
    if let Some(api) = inproc_python() {
        let expr = expr.to_string();
        let locals = locals.cloned();
        return api.run(move |c| c.eval_expr(&expr, locals.as_ref()));
    }
    let loc_wire = locals.map(val_to_wire).unwrap_or_else(|| json!({}));
    let raw = send_request(json!({
        "op": "eval",
        "expr": expr,
        "locals": loc_wire
    }))?;
    Ok(wire_to_val(&raw))
}

pub(crate) fn py_exec(code: &str) -> Result<Val, String> {
    if let Some(api) = inproc_python() {
        let code = code.to_string();
        return api.run(move |c| c.exec_code(&code));
    }
    let _ = send_request(json!({
        "op": "exec",
        "code": code
    }))?;
    Ok(Val::Null)
}

pub(crate) fn py_free(val: &Val) -> Result<bool, String> {
    let Some(id) = py_handle_id(val) else {
        return Ok(false);
    };
    if let Some(api) = inproc_python() {
        return api.run(move |c| c.free_handle(id));
    }
    let _ = send_request(json!({ "op": "release", "id": id }))?;
    Ok(true)
}

pub(crate) fn py_to_tensor(val: &Val) -> Result<Val, String> {
    if let Some(api) = inproc_python() {
        if is_py_handle(val) {
            let obj = val.clone();
            return api.run(move |c| c.export_to_tensor(&obj));
        }
    }
    if is_py_handle(val) {
        let raw = send_request(json!({
            "op": "tensor_export",
            "obj": val_to_wire(val)
        }))?;
        let shape: Vec<usize> = raw["shape"]
            .as_array()
            .map(|xs| {
                xs.iter()
                    .filter_map(|v| v.as_u64().map(|x| x as usize))
                    .collect()
            })
            .unwrap_or_else(|| vec![1]);
        let data: Vec<f64> = raw["data"]
            .as_array()
            .map(|xs| xs.iter().filter_map(|v| v.as_f64()).collect())
            .unwrap_or_default();
        return tensor_store(data, shape, false, TensorOp::Leaf);
    }
    Err("py_to_tensor expects a Python buffer/array/tensor handle".into())
}

pub(crate) fn tensor_to_py(tval: &Val) -> Result<Val, String> {
    if let Some(api) = inproc_python() {
        let tval = tval.clone();
        return api.run(move |c| c.import_from_tensor(&tval));
    }
    let id = tensor_id(tval)?;
    let snap = tensor_snapshot(id)?;
    let raw = send_request(json!({
        "op": "tensor_import",
        "shape": snap.shape,
        "data": snap.data
    }))?;
    Ok(wire_to_val(&raw))
}
