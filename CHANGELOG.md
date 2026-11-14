# Changelog

Consolidated release history for Kovrek, distilled from the milestone documents that
previously each lived in their own root-level file. Those files are retained for
provenance and are marked historical. Previously developed under the working name
*Veyra* prior to the 12.0 Sovereign Kovrek (`.kvr`, `.kvbc`) rebrand.

Two execution paths exist in this repository and are tracked separately below:

- **Native Horizon engine** — the Rust compiler/VM in `engine-rs/`, shipped as
  `bin/kovrek-native.exe`. This is the default path.
- **Convergence runtime** — the Python compatibility layer (`kovrek.py`,
  `kovrek_cli.py`). This is the legacy/interop path and the one that exposes
  `py_import`.

Release numbering below follows the native engine. Only Zenith and Nexus record a
release date in the source material; earlier entries are undated.

---

## 12.0 Sovereign

Self-hosting, language parity, compiler optimization, tensor broadcasting, and full-stack Sovereign Apex release.
Release gate verified **2026-11-14** (**130/130** unit tests, **74/74** packages, **8/8** release gates).

- **GPU Tiled/Attention Kernels & Low-Precision Dtypes (`spirv.rs`, `gpu.rs`, `tensor.rs`, `builtins/science_ml.rs`)**:
  - Added workgroup shared-memory tiled GEMM, Flash-style causal online-softmax attention, and fused Residual+LayerNorm+activation pipelines across SPIR-V, HLSL `cs_5_0`, and CPU fallback (`gpu_kernel_inspect`), plus IEEE-754 `f16`/`bf16` dtype casting.
- **Relational `@verify` Prover, `@linear` Move Enforcement & Auto-Arena Escape Analysis (`ir.rs`, `pipeline.rs`, `vm.rs`)**:
  - Upgraded `@verify` to a relational difference-bound solver with `if`/`else` lattice joins, half-bounded interval arithmetic, loop invariant + post-loop negation reasoning, callee `ensures` propagation, and concrete counterexample witnesses.
  - Extended `@linear` move checking into actor/channel/remote boundaries (`actor_send`, `actor_ask`, `actor_remote_send`, `actor_remote_ask`, `chan_send`) and added compile-time escape-analysis `auto_arena` reclamation.
- **Distributed TCP Actors, Crash-Safe Virtual FS, Flight-Recorder Profiler & Self-Hosted Peephole (`sim_actor.rs`, `vm.rs`, `tooling.rs`, `compiler-kvr/src/main.kvr`)**:
  - Added location-transparent TCP actor RPC (`actor_node_listen`, `actor_node_stop`, `actor_remote_send`, `actor_remote_ask`) and deterministic crash-safe fault-injectable VFS (`sim_fs_write`, `sim_fs_read`, `sim_fs_delete`, `sim_fs_list`, `sim_fs_sync`, `sim_fs_crash`, `sim_fs_snapshot`).
  - Added a zero-when-disabled production flight-recorder profiler (`profile_start`, `profile_stop`, `profile_report`) instrumenting both VM frames and JIT calls alongside upgraded `kovrek profile` GC/auto-arena reporting, plus pure-Kovrek arithmetic constant folding in `compiler-kvr/src/main.kvr` (preserving 3-stage bootstrap fixpoint).

- **IR Optimizer, Formal Taint, Linear Ownership & Interval Verifier (`ir.rs`, `pipeline.rs`)**:
  - Added SSA copy/constant propagation, algebraic identities, dead code elimination (`ir_stats`), formal `@secret`/`sanitize(...)` information-flow taint verification, `@linear`/`linear_consume(...)` ownership analysis across branches, and `@verify` static interval/bounds proofs.
- **WebAssembly/WASI & Direct Zero-Toolchain Native Binaries (`native_codegen.rs`, `cli.rs`)**:
  - Added standalone `.wasm` emission, disassembly, and stack VM execution with WASI linear-memory string & array lowering (`kovrek wasm [file.kvr] [-o out.wasm] [--disasm] [--run <fn> [args...]]`, `wasm_compile`, `wasm_disasm`, `wasm_run`).
  - Added direct zero-toolchain PE32+ and ELF64 x86_64 executable emission supporting heap/linear strings & arrays (`kovrek aot [--direct] [--elf|--pe]`).
- **Contracts, Static Effects & Capability Security (`pipeline.rs`, `runtime.rs`, `registry.rs`)**:
  - Added runtime `requires`/`ensures` contracts (`ContractError`) and transitive call-graph static effect verification for `@pure` and `@effect(...)`.
  - Added runtime capability sandboxing (`kovrek run --caps c1,c2`, `cap_current`, `cap_restrict`, manifest `"capabilities"`, `kovrek audit`) and reproducible `.kvpkg.json` bundles.
- **Heterogeneous Fused GPU Shaders, Fused Linear & Strided DLPack Interop (`spirv.rs`, `gpu.rs`, `py_bridge.rs`)**:
  - Extended `gpu_fuse` & `gpu_fuse_inspect` (`div`, `silu`, `clamp`, `pow_scalar`, `recip`), added single-pass fused GEMM+bias+activation (`gpu_fused_linear`) in SPIR-V & HLSL `cs_5_0`, and added `gpu_devices` hardware/software adapter reporting.
  - Added contiguous + arbitrary strided/non-contiguous PyCapsule `DLManagedTensor` / `DLManagedTensorVersioned` zero-copy tensor interop (`tensor_to_dlpack`, `py_from_dlpack`), verified with NumPy and PyTorch.
- **2-Gen Nursery/Old GC, `arena_scope`, Virtual-Clock Cluster & OTP Supervisors (`vm.rs`, `sim_actor.rs`)**:
  - Upgraded object GC to 2-generation nursery/old-gen with remembered-set write barriers (`gc_collect("minor"|"major")`, `gc_stats`, `gc_pause`, `gc_resume`) and deterministic LIFO `arena_scope(fn)`.
  - Added SplitMix64 virtual-clock discrete-event actor clusters (`sim_actor_spawn`, `sim_actor_send`, `sim_step`, `sim_run_until`, `sim_partition`, `sim_actor_state`, `sim_trace`) and OTP-style supervision trees (`supervisor_spawn` with `"one_for_one"`, `"one_for_all"`, `"rest_for_one"`, `supervisor_status`, `actor_mailbox_len`).
- **FFI Struct Bindgen, Dynamic C-ABI, Reversible DAP Watchpoints, DAG Notebooks & Native JIT (`plugin.rs`, `dap.rs`, `server.rs`, `jit.rs`)**:
  - Added C header `struct`/`enum`/`#define` bindgen (`kovrek ffi bindgen`), dynamic multi-arg x86_64 C-ABI calls (`ffi_call`), and bounded `mem_*` linear heap buffers.
  - Added time-travel reversible DAP with forward & reverse data watchpoints (`stepBack`, `reverseContinue`, `dataBreakpointInfo`, `setDataBreakpoints`), AST dependency-driven topological DAG notebooks with cycle detection + `/api/notebook/dag` + `/api/notebook/export`, and native JIT dynamic `[..]` lists + `log2`/`hypot`/`fmod`/`atan2` + `Op::SetIndex` + `NumList` returns.

- **Self-Hosted Compiler (`compiler-kvr/src/main.kvr`) & `.kvbc` Bytecode**:
  - Added portable `kovrek-bc-v1` (`.kvbc`) bytecode serialization with SHA-256 tamper
    detection (`encode_bytecode`, `decode_bytecode`, `compile_to_kvbc`, `run_kvbc`,
    `kvbc_compile`, `kvbc_run`, and `.kvbc` CLI execution/output).
  - Implemented a 100% pure-Kovrek tokenizer, recursive-descent parser, and `.kvbc`
    bytecode generator in `compiler-kvr/src/main.kvr`, verified to a 3-stage bootstrap
    fixpoint (`Stage 2 .kvbc == Stage 3 .kvbc`).
- **Engine Architecture & Builtin Decomposition (`Phase 1`)**:
  - Decomposed `engine-rs/src/vm.rs` (4,716 lines → ~780 lines) by extracting all
    210+ built-ins into `engine-rs/src/builtins/{mod,core_collections,math_text_fs,science_ml,data_gui,sys_net_conc_ffi}.rs`.
  - Separated `engine-rs/src/lib.rs` from the 5-line `engine-rs/src/main.rs` binary
    entry point.
- **Language Semantics, Closures, Unwind Safety & Syntax Parity (`Phase 2`)**:
  - Added per-variable `Arc<Mutex<Val>>` closure upvalue cells (`Vm::cells` and
    `FunctionDef::upvalues`) so closures maintain independent mutable lexical state
    across invocations and sync mutations with enclosing scopes.
  - Added short-circuit `and`/`or` compilation, unwind-safe `try`/`catch` and
    `with ... as ...` context manager cleanup on exceptions, line-aware lexer/parser/LSP
    diagnostics, and static parent-class existence checks.
  - Achieved full syntax parity across runtimes: `/* ... */` and `#` comments, `'...'`
    single-quoted strings, right-associative `**`, tuple `(a, b)` and set `{a, b}`
    literals, `else if` chains, `+=`/`-=`/`*=`/`/=`, list/map index assignment
    (`xs[i] = v`, `m[k] = v`), and 1/2/3-arg `range(...)`.
- **Bytecode Optimizer & Tensor Runtime (`Phase 3`)**:
  - Added `engine-rs/src/ir.rs` (`optimize_bytecode`) performing jump-target-safe
    constant folding, double-negation elimination, dead branch elimination, and jump
    threading.
  - Added NumPy/PyTorch-style N-D tensor broadcasting with automatic backward
    gradient reduction across broadcasted axes, cache-friendly row-major
    `tensor_matmul`, and unreachable autograd tape garbage collection
    (`tensor_gc_unreachable`, `tensor_gc`, `tensor_live_count`).
- **Security, Tooling & Release Gates (`Phase 0` & `Phase 4`)**:
  - Patched static server path traversal (`resolve_static_path`), multi-`use` line
    truncation in `load_source`, and `import_package` function reference sharing.
  - Upgraded `INSTALL.bat` to install the native Horizon binary, upgraded
    `scripts/package-matrix.ps1` and `scripts/package-matrix.sh` to execute all 60
    packages, added `examples/zenith/autograd_adam.kvr` and
    `examples/nexus/security_and_data.kvr`, and achieved 72/72 unit tests and 8/8
    passing release gates.

## 11.0 Nexus

Ecosystem-focused successor to Zenith. Release gate reported **2026-09-21**.

- Registry expanded 52 → **60 native packages**; added kvarray, kvvalidate, kvauth,
  kvquery, kvserde, kvetl, kvmodel, kvapi.
- Hosted registry support via `KOVREK_REGISTRY_URL`, with HTTP download for
  single-file and multi-file packages.
- Recursive package dependencies with cycle protection, plus path-traversal
  protection on downloaded files. Local/offline registry remains the default.
- Lockfile SHA-256 integrity auditing remains mandatory.
- Tensor stack: division with reverse-mode gradients, negation, scalar power,
  numerically stable row-wise softmax (1D/2D) with gradients, `detach`, `numel`.
- Persistent Adam optimizer state with bias correction and reset.
- Web runtime: app handlers receive request headers and may return arbitrary
  response headers (cookies, CORS, cache control, authorization, request IDs).
- LSP and VS Code completion surfaces extended for the new tensor and task APIs.
- New ecosystem release gate covering hosted registry fetch, recursive dependency
  install, compile/execute of downloaded packages, lockfile audit, request-header
  delivery and custom response headers over real HTTP.

Reported gate result: fmt, 57/57 tests, portable-core check, clippy with warnings
denied, release build, 60/60 package matrix, hosted-registry gate, remote run with
SHA-256 audit, HTTP header gate, installed CLI `11.0.0-nexus`, installed extension
`kovrek.kovrek-language@11.0.0`, package tamper detection and restore.

Independently re-verified **2026-10-10** with `cargo`/`rustc` 1.93.1 on Windows at
git `d92996b`: all eight gates in `scripts/gate-report.ps1` pass — fmt, 57/57 tests,
portable-core check, clippy with warnings denied, release build, 60/60 package
matrix, ecosystem gate and release check. The installed-CLI and extension-version
checks in the list above were not part of that re-run. See [STATUS.md](STATUS.md).

## 10.0 Zenith

Declared the Kovrek 10 engineered roadmap complete. Release gate reported **2026-09-21**.

- Typed functions, recursion, closures, escaping function objects.
- Classes, inheritance, runtime type enforcement.
- Lists, maps, sets, tuples, comprehensions, general iteration.
- Generators, decorators, `match`/`case`, structured exceptions.
- `with`/`as` and `finally` cleanup semantics.
- Async functions, native tasks, `gather`, channels, timeout-aware waits,
  runtime task status inspection.
- C ABI FFI; package namespaces via `import_package()`.
- Services: filesystem, paths, processes, environment, platform introspection,
  JSON, TOML, CSV, Base64, UUID, date/time.
- HTTP clients plus static, routed and dynamic application servers.
- SQLite with parameter binding and ORM foundations.
- Security: SHA-256, HMAC-SHA256, PBKDF2, constant-time equality.
- Dataframe transforms, joins, grouping, null handling, summaries.
- N-dimensional tensor handles with reverse-mode autograd, trainable parameters,
  gradient descent, `zero_grad`, serialization/loading without Python.
- Native LSP: completion, hover, definitions, references, rename, symbols,
  formatting, diagnostics.
- 52-package native registry; Windows/Linux/macOS CI; tagged release workflow with
  staged platform bundles and SHA-256 manifests; portable-core build gate.

The "100%" label in the original document applies to this repository's own roadmap
and gates, not to Python/C/JavaScript ecosystem size, age or adoption.

## 9.0 Ascendant

Depth-focused release rather than surface syntax.

- Escaping closures became real exported callable objects; decorators can return
  newly created nested closures that outlive the creating VM.
- Runtime package namespaces via `import_package("name")` returning isolated module
  objects; namespaced method dispatch prevents global symbol collisions.
- Native tensor runtime with shapes, values and trainable parameters; reverse-mode
  autograd for add/sub/mul/ReLU/sum/mean/matmul; verified matmul gradients;
  gradient-descent updates and `zero_grad`; full tiny-model training inside Kovrek.
- Parameterized SQLite exec/query primitives; ORM packages upgraded for bound
  parameters.
- HMAC-SHA256, PBKDF2-SHA256, constant-time equality.
- Native LSP rename support.
- Fixed loop re-execution of non-const local declarations.
- Registry expanded to 52 packages.

Reported quality gates: 52/52 regression tests, 52/52 packages install/import/check,
fmt, portable-core check, clippy with warnings denied.

## 8.0 Elevation

First release aimed at a defensible ~50%-overall comparison, with ecosystem maturity
reported separately.

- `async fn` and async methods; `await` + `gather`.
- `with`/`as` context management; decorator syntax; `try`/`catch`/`finally`.
- Parameterized web routes and query parameters; request/response helpers in kvweb.
- Dataframe head/tail, drop/fill nulls, left joins.
- ML workflow metrics: accuracy, precision, recall, F1, confusion counts,
  train/test split.
- Expanded path/text services.
- 47 native packages; package-matrix automation on Windows and POSIX; generated
  package index documentation.

Reported gates: 45/45 tests, 47/47 packages, fmt, portable-core check, clippy,
live parameterized routing, package integrity hashing, Python-independent execution.

## 7.0 Meridian

First release to claim the ~50%-class technical threshold.

- Generators (`yield`, `next`, completion checks, collect) usable in `for`-`in`.
- First-class lambdas and named function values; `map`/`filter`/`reduce` accept
  Function values.
- List comprehensions with optional `if` filtering; `match`/`case` with wildcard.
- Generic HTTP `request(method, url, headers, body, timeout)`.
- Platform introspection (OS, architecture, family, path separator).
- TOML parse/stringify; UTC/local datetime helpers; Base64; UUID v4;
  regex match/replace/split; set union/intersection/difference.
- Determinant, inverse, linear-system solving; grouped data sum/mean; histogram,
  scatter and bar visualization.
- CLI: `run`/`check`/`lint`/`fmt`/`test`/`build`/`repl`/`disasm`,
  `ast`/`symbols`/`debug`, `bench`/`profile`/`doc`,
  `registry`/`add`/`remove`/`install`/`publish`/`audit`, `serve`/`web`/`app`, `lsp`.
- LSP: document sync, diagnostics, completion, hover, symbols, formatting,
  go-to-definition, references.
- 30 native packages with a fresh-project add/import/check matrix; content-hashed
  lockfiles; tamper-detecting audit.
- Feature-gated native services; cross-platform CI matrix (Windows, Ubuntu, macOS).
  38 native unit/regression tests.

Platform readiness in this release is build readiness, not certification on physical
Linux/macOS/ARM hardware.

## 6.0 Apex

- Native classes, inheritance, constructors, fields, bound methods,
  reference-semantic objects.
- Runtime-enforced type annotations for variables, assignments, parameters, returns
  and user-defined class types.
- Lexical semantic checking for undefined names and duplicate declarations;
  lexical captures for nested functions.
- `try`/`catch`/`throw`; general iteration over lists and strings; `break`/`continue`.
- Native `await` over Kovrek tasks; background tasks via `spawn`/`task_done`/
  `await_task`; message-passing channels.
- Direct C ABI dynamic-library calls for i32/i64/f64; CPU-count and sleep primitives.
- Static, routed and dynamic web applications; stateful objects across requests;
  native HTTP GET/POST.
- SQLite, JSON, regex, files, environment, subprocess, SHA-256, GUI services.
- Vectors and matrices; mean/variance/stddev/median/percentile/covariance/
  correlation; CSV and row-map tables; column/filter/describe/group/sort/select/
  inner-join; SVG plotting; linear and logistic regression, k-NN, k-means,
  normalization, dense inference, ReLU, sigmoid, softmax, MSE.
- Toolchain: `new`/`run`/`check`/`fmt`/`test`/`build`/`disasm`/`repl`, `lint`/`bench`/
  `profile`/`doc`, registry search/add/remove/install/publish, content-hashed
  lockfiles, package-integrity audit.
- 30 tests covering legacy runtime behavior plus classes, inheritance, exceptions,
  lexical captures, type enforcement, FFI, tasks, channels, `await`, functional
  collections, and science/data/ML services.

## 5.0 Frontier

The 50%-class practical capability milestone, measured per category.

- Core language/runtime ~75%, project/tooling ~70%, standard services ~65%,
  data/scientific ~55%, AI/ML ~50% (introductory foundation), web ~50%
  (small-app/server foundation), GUI ~50% (utility dialogs, not a widget toolkit),
  database ~55%, package ecosystem ~55% (infrastructure, not catalogue size).
- Seeded packages: kvmath, kvstats, kvalgo, kvcollections, kvtext, kvtest, kvhello,
  kvscience, kvml, kvdata, kvplot, kvweb, kvgui.

## 4.0 Sovereign

The 50% independent-ecosystem milestone: roughly half of the previously
Python-dependent platform categories gained a usable Kovrek-owned path.

- Native Windows executable, lexer/parser/AST, bytecode compiler, stack VM,
  user functions and recursion, constants, control flow, lists, maps, indexing,
  module loading.
- Standard services independent of Python: JSON, regex, text/math utilities,
  files/directories/paths, environment, subprocesses, HTTP GET, random/time,
  SHA-256, SQLite.
- Project tooling: creation, manifests, checking, formatting, test runner,
  distribution builds, disassembly, REPL.
- Package ecosystem: Kovrek-owned registry, search/list, add/remove, vendoring,
  lockfiles, restore/install, local publishing, package-name module resolution,
  seven seeded packages.
- Editor path switched to the native executable rather than Python.

Verified by running a built distribution with Python absent from `PATH`:
`Kovrek 4.0.0-sovereign`, native package checks passing, `ECOSYSTEM_OK`, exit 0. The
package lifecycle was verified end to end (publish → search → add → use by name →
run → delete vendor → restore → run → build).

## Convergence runtime (Python path)

The Python compatibility layer carries its own version string, currently
`3.2.0-convergence`, in both `kovrek.py` and `kovrek_cli.py`. `kovrek_cli.py` imports
`kovrek` explicitly as `legacy` and shells out to `bin/kovrek-native.exe` for native
commands.

Measurements recorded against this path while it was the primary runtime:

- **90%-practical milestone** — general scripting, typed functions and recursion,
  OOP, structured exceptions, core collections, filesystem/environment, JSON and
  regex, HTTP clients, SQLite and database access, thread-pool tasks and concurrent
  mapping, plus scientific, AI/ML, web, image and native-extension ecosystems
  reached through `py_import`; isolated project environments; dependency
  add/remove/list/install/lock/audit; formatter, linter, semantic checker, tests,
  benchmark, profiler, trace; API documentation; REPL; VS Code support.
- **Python parity 50%** and **Python overall 50%** — split scoring that separates
  engineered technical capability from whole-ecosystem comparison.
- **Python 75% target** — Ascendant-era position of roughly 78–82% on engineered
  technical capability, with the explicit caveat that this is not 75% of the entire
  Python world.

All of these documents consistently exclude ecosystem-scale dimensions (PyPI
package count, users, maintainers, production history, documentation volume,
research adoption, commercial support) from their technical scores, and report them
separately as still far below Python.
