# Kovrek Universal Runtime & Interop Architecture

Kovrek 12.0+ ships with a universal polyglot, OS, concurrency, Web/SSE, and Electron-class desktop application layer designed to integrate cleanly with Node.js/npm, Python/PyPI, C/C++ FFI, WebAssembly, native GPU kernels, and browser WebViews without giving up Kovrek's formal verifier (`@verify`), capability enforcement (`--caps`), or taint analysis.

## 1. Universal Filesystem & Environment Primitives
All filesystem and environment operations participate in Kovrek's `fs` / `fs.read` / `fs.write` / `process` capability gates:
- `fs_mkdir(path) -> Bool`: recursive directory creation (`mkdir -p` semantics).
- `fs_list(path) -> List<Map>`: sorted directory entries (`name`, `path`, `is_dir`, `is_file`, `size`).
- `fs_walk(path, max_depth) -> List<Map>`: bounded recursive traversal excluding noisy VCS/dependency trees (`.git`, `node_modules`, `target`).
- `fs_remove(path) -> Bool`: removes a file or directory recursively.
- `fs_stat(path) -> Map | Null`: metadata (`path`, `is_dir`, `is_file`, `size`, `modified_ms`, `readonly`).
- `fs_read_bytes(path) -> List<Number>` and `fs_write_bytes(path, bytes) -> Number`.
- `fs_read_base64(path) -> String`: binary file encoding for multimodal/image payloads.
- `env_set(key, value) -> Bool` and `env_vars() -> Map`.

Standard package wrapper: `use "kvfs";` (`packages/kvfs/kvfs.kvr`).

## 2. Bidirectional Supervised OS Process Engine
- `proc_spawn(cmd, args_list, opts_map) -> pid` (`opts_map` supports `cwd`, `env`).
- `proc_read(pid) -> { running, exit_code, out, err }`: drains non-blocking stdout/stderr ring buffers.
- `proc_write(pid, text) -> Bool`: writes directly to child `stdin` and flushes.
- `proc_status(pid) -> { running, exit_code }`.
- `proc_kill(pid) -> { killed, exit_code }`.

Standard package wrapper: `use "kvproc";` (`packages/kvproc/kvproc.kvr`).

## 3. Native Node.js / npm Bridge & `kovrek pkg js-bridge`
Kovrek automatically locates `node` (including `PATH`, `C:\Program Files\nodejs`, `C:\nvm4w\nodejs`) and `npm` / `npm.ps1`:
- `node_available() -> Bool`
- `node_info() -> { available, binary, version, npm_binary }`
- `node_eval(js_code) -> Any`: executes JavaScript inside a persistent async-capable Node.js IPC worker (`vm` / JSON-line protocol).
- `node_require(mod_name) -> Any`: inspects exports from any built-in Node module or local npm dependency.
- `node_call(mod_name, fn_name, args_list) -> Any`: calls Node functions (sync or Promise/async) and marshals JSON results back to Kovrek values.
- `npm_run(args_list, cwd) -> { code, out, err, elapsed_ms }`.
- CLI generator: `kovrek pkg js-bridge <npm_pkg> [out_dir] [--npm-install]` scaffolds a ready-to-import `js_<pkg>` package wrapper.

Standard package wrapper: `use "kvnode";` (`packages/kvnode/kvnode.kvr`).

## 4. Concurrent HTTP/SSE App Server & Streaming Client
- `http_stream(method, url, headers_map, body_or_null, timeout_ms, on_line_fn) -> { status, ok, lines }`: streams HTTP response lines live into a Kovrek callback (returning `false` stops early). Ideal for live token streaming from Ollama/OpenAI-compatible backends.
- `app_serve(handler_fn, options_map)`: multi-worker HTTP/SSE server (`port`, `workers`, `static_dir`, `max_requests`, `on_ready`) using `Vm::clone_worker()`. Returning `{"stream_channel": ch_id}` streams `text/event-stream` SSE events automatically from any background `spawn` task via `send(ch_id, msg)`.
- `kovrek app <app.kvr> <routes.json> [port]` now uses a concurrent 4-worker thread pool (configurable via `"workers"`), optional `"static_dir"` in `routes.json`, and automatic `stream_channel` SSE streaming.

## 5. Electron-Class Desktop WebView Shell (`desktop_app`)
- `desktop_app(spec_map[, handler_fn])`:
  - Spec supports `title`, `width`, `height`, `port`, `workers`, `dir`, `html`, `url`, `headless`, `max_requests`, `on_ready`.
  - Automatically serves `/kovrek-ipc.js` (`window.kovrek.invoke(action, payload)`, `window.kovrek.health()`, `window.kovrek.shutdown()`), `/__kovrek/health`, `/__kovrek/invoke`, `/__kovrek/shutdown`, and any custom HTTP/SSE route passed to `handler_fn(req)`.
  - Launches Edge / Chrome in `--app=<url> --user-data-dir=<isolated_profile>` desktop mode and ties the backend server lifetime cleanly to the window process exit.

Standard package wrapper: `use "kvdesktop";` (`packages/kvdesktop/kvdesktop.kvr`).

## 6. Rich Native Win32 `gui_app` Additions
- `{"type": "memo", "id": "...", "height": 220, "text": "..."}`: scrollable word-wrapped multiline viewer supporting mouse-wheel scrolling.
- `"timer_ms": 250` on root layout spec: fires `{ "kind": "tick" }` events into `on_event` so pure native Win32 apps can drive live progress bars and background task telemetry without Electron/browser overhead.

## 7. Unified Discovery & Polyglot Dispatch
- `runtime_info() -> Map`: reports `kovrek_version`, `platform`, `arch`, `capabilities`, `shells`, `node`, `python`, `jit`, `gpu`, and `webview`.
- `bridge_call(runtime, target, payload) -> Any`: uniform bridge entrypoint across `"node"`, `"shell"`, `"powershell"`, `"bash"`, and `"verify"`.
