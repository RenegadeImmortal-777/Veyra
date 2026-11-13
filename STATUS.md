# Veyra Status

Single authoritative statement of where this project currently stands. If this file
and any other document disagree, this file and [CHANGELOG.md](CHANGELOG.md) win. The
older `STATUS-*.md` and `PYTHON-*.md` files are historical records, retained for
provenance and marked as such.

## Current release

**Veyra 12.0 "Sovereign"** — the native Rust Horizon engine + self-hosted Veyra
compiler (`compiler-vy`), verified on **2026-11-08**.

| Where | Value |
|---|---|
| `engine-rs/src/lib.rs` (`VERSION` const) | `12.0.0-sovereign` |
| `engine-rs/Cargo.toml` | `12.0.0` |
| Native launcher | `bin/veyra-native.exe`, invoked by `veyra.bat` and installed by `INSTALL.bat` |
| Self-hosted compiler | `compiler-vy/src/main.vy` (3-stage `.vybc` bootstrap fixpoint verified) |
| VS Code extension | `tooling/vscode/package.json` (`12.0.0`) |
| Legacy Python runtime | `3.2.0-convergence` (`veyra.py`, `veyra_cli.py`) |

## Two execution paths

This repository contains two runtimes. Both are real; confusing them is the easiest
way to misread the project.

1. **Native Horizon engine** — Rust lexer → parser → AST → bytecode compiler → IR
   optimizer (`ir.rs`) → stack VM (`vm.rs`) and `.vybc` bytecode serializer
   (`bytecode.rs`), in `engine-rs/`. This is the default execution path and does not
   require Python. All current release claims refer to this engine.
2. **Convergence runtime** — the Python compatibility layer. `veyra_cli.py` imports
   `veyra` explicitly as `legacy` and delegates native commands to
   `bin/veyra-native.exe`. This path is what exposes `py_import` and the wider
   Python package ecosystem.

## Verification status

Last full gate run: **2026-11-13**. Every gate passed (**8/8**).

| Gate | Result |
|---|---|
| `cargo fmt --check` | PASS |
| `cargo test` | PASS — 85 passed, 0 failed |
| `cargo check --no-default-features` | PASS |
| `cargo clippy --all-targets --all-features -- -D warnings` | PASS |
| `cargo build --release` | PASS |
| `scripts/package-matrix.ps1` | PASS — 60/60 packages checked & executed |
| `scripts/ecosystem-gate.ps1` | PASS |
| `scripts/release-check.ps1` | PASS (`bin/veyra-native.exe` synced) |

Environment of that run: Windows, `cargo`/`rustc` 1.93.1, Python 3.12.10. Raw output
and per-gate exit codes are written to `gate-report.txt`.

The engine source is modularized under `engine-rs/src/`:
- `main.rs` is a 5-line binary entry point calling `veyra::run_cli()` in `lib.rs`.
- `vm.rs` implements the flat `CallFrame` execution stack, indexed local variable
  slots (`LoadLocal`/`DefineLocal`/`StoreLocal`), loop/indexing superinstructions, shared
  method cache, and `Arc<Mutex<Val>>` closure upvalue cell management, while built-in
  operations live in `engine-rs/src/builtins/{mod,core_collections,math_text_fs,science_ml,data_gui,sys_net_conc_ffi}.rs`.
- `aot.rs` implements native AOT compilation (`veyra aot`, `veyra build --native`) via
  standalone optimized Rust codegen compiled by `rustc` (supporting `async fn`/`await`/`gather`,
  `yield` generators + generator expressions, `**kwargs`/chained calls, dunders, and Python interop).
- `py_bridge.rs` provides a persistent stdio JSON-RPC bridge to Python 3 (`py_available`,
  `py_import`, `py_call`, `py_get`, `py_set`, `py_dir`, `py_eval`, `py_exec`) with transparent
  member, method, index, and call handle routing across VM and native AOT binaries.
- `dap.rs` and `lsp.rs` implement the native DAP debugger and LSP server consumed by
  the VS Code extension (`tooling/vscode/`).
- See [docs/ENGINE-SPLIT.md](docs/ENGINE-SPLIT.md) and [WAY_FORWARD.md](WAY_FORWARD.md)
  for the full architectural map.

### Re-running the gates

From the repository root:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\gate-report.ps1
```

This runs every gate above in order, preserves each exit code instead of hiding it
behind a pipe, writes `gate-report.txt`, and exits non-zero if any gate fails.

## What the platform does

Native, without Python:

- **Language**: typed functions and recursion, default parameters and `*rest`
  varargs, ternary expressions (`a if cond else b`), closures with shared mutable
  lexical upvalues (`Arc<Mutex<Val>>` cells), classes with inheritance and static/runtime
  type enforcement, unwind-safe `try`/`catch`/`finally` and `with ... as ...`
  context managers, generators, decorators, list comprehensions, `match`/`case`,
  short-circuit `and`/`or`, right-associative `**`, tuple `(a, b)` and set `{a, b}`
  literals, f-strings, compound assignments (`+=`, `-=`, `*=`, `/=`), list/map index
  mutation (`xs[i] = v`, `m[k] = v`), 1/2/3-arg `range(...)`, async functions with
  `await`/`gather`, tasks, channels, timeout-aware waits, indirect/chained calls (`f(x)(y)`),
  keyword arguments/`*args`/`**kwargs`, chained comparisons (`0 <= x < 10`), lazy generator
  expressions (`(x * x for x in xs)`), class dunders (`__add__`, `__sub__`, `__mul__`, `__eq__`,
  `__getitem__`, `__setitem__`, `__len__`, `__str__`, `__repr__`, `__call__`), expanded Python-parity
  builtins/methods (`repr`, `callable`, `divmod`, `getattr`, `hasattr`, `setattr`, `sorted`/`min`/`max`
  with `key=`, variadic `zip`, `enumerate(..., start=...)`, `print(..., sep=..., end=...)`), and live
  Python 3 library interop (`py_import`, `py_call`, `py_get`, `py_set`, `py_dir`, `py_eval`, `py_exec`).
- **Bytecode, VM & Native AOT**: portable `veyra-bc-v1` (`.vybc`) bytecode
  serialization with SHA-256 tamper verification, multi-pass bytecode optimizer
  (`ir.rs`), flat `CallFrame` + indexed local slot VM (`vm.rs`), pure-Veyra
  self-hosted compiler (`compiler-vy/src/main.vy`) verified to a 3-stage bootstrap
  fixpoint (`Stage 2 .vybc == Stage 3 .vybc`), and full-parity native AOT compilation (`aot.rs`,
  `veyra aot`, `veyra build --native`) including async, generators, dunders, and Python bridge.
- **Runtime services**: JSON, TOML, CSV, regex, Base64, UUID, date/time, filesystem,
  paths, processes, environment, platform introspection.
- **Application**: HTTP client, path-traversal-hardened static/routed/dynamic web
  servers, request and response headers, SQLite with parameter binding, ORM
  foundations.
- **Security**: SHA-256, HMAC-SHA256, PBKDF2, constant-time equality, lockfile
  integrity audit with tamper detection.
- **Data/ML**: dataframe transforms and joins, statistics, linear algebra,
  regression, clustering, ML metrics, plus an N-dimensional tensor runtime with
  NumPy/PyTorch-style N-D broadcasting and backward gradient reduction, cache-friendly
  `tensor_matmul`, reverse-mode autograd with automatic unreachable tape GC
  (`tensor_gc_unreachable`), Adam and SGD optimizers, neural network / transformer
  layers (`tensor_scale`, `tensor_cross_entropy`, `tensor_layer_norm`, `tensor_conv1d`,
  `tensor_conv2d`, `tensor_attention`, `tensor_multihead_attention`), and checkpoint
  save/load serialization.
- **Packages**: an offline registry by default, hosted registry over HTTP, recursive
  dependencies with cycle protection, 60 shipped packages verified both statically
  and at runtime by `scripts/package-matrix.ps1`.
- **Tooling**: `run`/`check`/`lint`/`fmt`/`test`/`build`/`aot`/`repl`/`disasm`,
  `bench`/`profile`/`doc`, line-aware LSP diagnostics and navigation (completion,
  hover, definitions, references, rename, hierarchical symbols, formatting), full
  Debug Adapter Protocol server (`veyra dap` with breakpoints, stepping, stack frames,
  variable inspection, and `evaluate`), and the VS Code extension (`tooling/vscode/`).

## Resolved inconsistencies (closed in Veyra 12.0)

- `INSTALL.bat` now installs the native Horizon engine (`bin/veyra-native.exe`) as
  the primary `veyra` CLI command and exposes the legacy Python bridge as
  `veyra-py`.
- `examples/zenith/autograd_adam.vy` and `examples/nexus/security_and_data.vy` have
  been added and verified.
- Phase 2 of [docs/ENGINE-SPLIT.md](docs/ENGINE-SPLIT.md) is complete: `vm.rs` is
  split into `engine-rs/src/builtins/` and `lib.rs` is separated from `main.rs`.
- `compiler-vy/src/main.vy` implements the pure-Veyra self-hosted `.vybc` compiler
  verified to a 3-stage fixpoint.

## Remaining long-horizon directions

- Optional LLVM/Cranelift JIT tier (`aot.rs` now covers async, generators, kwargs, dunders, and Python interop natively via `rustc` AOT).
- GPU tensor backend (CUDA/Vulkan/wgpu).
- In-process CPython C-API `.pyd`/`.so` ABI loading and a hosted, cryptographically signed community package registry at scale (access to installed Python modules today goes through the out-of-process `py_bridge` Python 3 worker).

## Repository map

| Path | Contents |
|---|---|
| `engine-rs/` | Native Rust Horizon engine (`lib.rs`, `main.rs`, `builtins/`, `ir.rs`, `vm.rs`, etc.) |
| `compiler-vy/` | Pure-Veyra self-hosted compiler emitting `veyra-bc-v1` (`.vybc`) |
| `bin/` | Built native launcher (`bin/veyra-native.exe`) |
| `packages/` | 60 native Veyra packages |
| `registry/` | Registry index |
| `stdlib/` | Veyra-written standard library modules |
| `spec/` | Architecture and language specification |
| `docs/` | Language guide, engine split map, package index, remote registry notes |
| `examples/` | Per-release example programs (`elevation/`, `ascendant/`, `apex/`, `zenith/`, `nexus/`) |
| `scripts/` | Release, package-matrix and ecosystem gates |
| `tooling/` | VS Code extension and bundles |
| `tests/` | Python-runtime compatibility test suite |
| `veyra.py`, `veyra_cli.py` | Legacy Convergence runtime and CLI |
| `veyra.bat`, `INSTALL.bat` | Native launcher and installer scripts |
| `CHANGELOG.md` | Consolidated release history |
| `WAY_FORWARD.md` | Technical audit and 6-phase execution roadmap |
