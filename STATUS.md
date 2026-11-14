# Veyra Status

Single authoritative statement of where this project currently stands. If this file
and any other document disagree, this file and [CHANGELOG.md](CHANGELOG.md) win. The
older `STATUS-*.md` and `PYTHON-*.md` files are historical records, retained for
provenance and marked as such.

## Current release

**Veyra 12.0 "Sovereign"** — the native Rust Horizon engine + self-hosted Veyra
compiler (`compiler-vy`), verified on **2026-11-14**.

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

Last full gate run: **2026-11-14**. Every gate passed (**8/8**).

| Gate | Result |
|---|---|
| `cargo fmt --check` | PASS |
| `cargo test` | PASS — 130 passed, 0 failed |
| `cargo check --no-default-features` | PASS |
| `cargo clippy --all-targets --all-features -- -D warnings` | PASS |
| `cargo build --release` | PASS |
| `scripts/package-matrix.ps1` | PASS — 74/74 packages checked & executed |
| `scripts/ecosystem-gate.ps1` | PASS |
| `scripts/release-check.ps1` | PASS (`bin/veyra-native.exe` synced) |

Environment of that run: Windows, `cargo`/`rustc` 1.93.1, Python 3.12.10. Raw output
and per-gate exit codes are written to `gate-report.txt`.

The engine source is modularized under `engine-rs/src/`:
- `main.rs` is a 5-line binary entry point calling `veyra::run_cli()` in `lib.rs`.
- `ir.rs` & `pipeline.rs` provide the SSA propagation / algebraic simplification / DCE optimizer
  (`optimize_bytecode`, `ir_stats`), compile-time escape-analysis `auto_arena` inference, transitive
  `@pure`/`@effect(...)` checking, formal `@secret`/`sanitize(...)` taint-flow verification,
  `@linear`/`linear_consume(...)` ownership analysis across calls/actors/channels/remotes, and the
  relational difference-bound `@verify` solver with lattice joins, loop invariants, callee
  `ensures` propagation, and concrete counterexample synthesis.
- `vm.rs` implements the flat `CallFrame` execution stack, indexed local variable
  slots (`LoadLocal`/`DefineLocal`/`StoreLocal`), loop/indexing superinstructions, shared
  method cache, 2-generation nursery/old-gen cycle-collecting object GC with remembered-set
  write barriers (`gc_collect("minor"|"major")`, `gc_stats`, `gc_pause`, `gc_resume`),
  deterministic LIFO `arena_scope(fn)` + automatic escape-analysis `auto_arena` scopes, a
  zero-when-disabled production flight-recorder profiler (`profile_start`/`profile_stop`/
  `profile_report`, tracking VM frames and JIT calls), automatic hot-function promotion into
  `jit.rs`, and `Arc<Mutex<Val>>` closure upvalue cell management, while built-in operations live in
  `engine-rs/src/builtins/{mod,core_collections,math_text_fs,science_ml,data_gui,sys_net_conc_ffi}.rs`.
- `jit.rs` provides Veyra's in-memory **multi-architecture x86_64 & aarch64 native machine-code JIT compiler**
  (`W^X` executable allocation, CFG type/stack verification, self-recursion `call rel32`, inter-function calls,
  dynamic `[..]` list allocation via `jit_alloc_list`, list indexing/slicing, `Op::SetIndex` list mutation and
  `NumList` returns, and extended math helpers including `log2`, `hypot`, `fmod`, `atan2`).
- `gpu.rs` and `spirv.rs` provide Veyra's **universal Vulkan SPIR-V & Direct3D 11 (`cs_5_0` HLSL) `f64`/`f32` GPU compute
  backend** (`vulkan-1.dll`/`libvulkan.so.1`, `dxgi.dll`, `d3d11.dll`, `d3dcompiler_47.dll` with multi-threaded
  CPU fallback), rich adapter discovery (`gpu_devices`), single-pass fused elementwise kernel generation
  (`gpu_fuse`, `gpu_fuse_inspect` including `div`, `silu`, `clamp`, `pow_scalar`, `recip`), fused GEMM+bias+activation
  (`gpu_fused_linear`), workgroup shared-memory tiled GEMM, Flash-style causal online-softmax attention, fused
  Residual+LayerNorm+activation (`gpu_kernel_inspect`), IEEE-754 `f16`/`bf16` dtype casting, custom HLSL
  (`gpu_run_hlsl`), and `tensor_to(t, "gpu")` device execution.
- `native_codegen.rs` and `aot.rs` implement standalone WebAssembly (`.wasm`) compilation/disassembly/execution
  with WASI linear-memory strings & arrays (`veyra wasm [file.vy] [-o out.wasm] [--disasm] [--run <fn> [args...]]`,
  `wasm_compile`/`wasm_disasm`/`wasm_run`), direct zero-toolchain PE32+/ELF64 binary emission including
  strings & arrays (`veyra aot [--direct] [--elf|--pe]`), and full-parity `rustc`-compiled AOT builds (`veyra aot`, `veyra build --native`).
- `plugin.rs` and `registry.rs` implement Veyra's `veyra_plugin_v1` native `cdylib`
  extension ABI, pure-Rust C header FFI bindgen with `struct`/`enum`/`#define` layout lowering,
  dynamic multi-arg x86_64 C-ABI trampoline (`ffi_call`), and bounded linear heap buffers (`mem_*`),
  HMAC-SHA256 package signing (`keygen`/`sign`/`verify`), reproducible `.vypkg.json` bundles
  (`pack`/`unpack`), scoped `@org/pkg` + semver constraints, `veyra.lock` format 3,
  federated registries (`VEYRA_REGISTRY_URLS`) with offline cache (`VEYRA_OFFLINE=1`), monorepo
  `"workspaces"`, `veyra pkg py-bridge`, and HTTP registry publishing/serving (`veyra pkg serve`, `POST /publish`).
- `py_bridge.rs` provides in-process CPython C-API bindings (`python3xx.dll`/`libpython3`, zero-copy PEP 3118
  buffer `py_to_tensor`, and PyCapsule DLPack `DLManagedTensor` / `DLManagedTensorVersioned` contiguous +
  strided/non-contiguous interop via `tensor_to_dlpack` / `py_from_dlpack`) with subprocess fallback across VM and native AOT binaries.
- `sim_actor.rs` implements SplitMix64 deterministic simulation, crash-safe fault-injectable virtual
  filesystems (`sim_fs_write`/`sim_fs_read`/`sim_fs_delete`/`sim_fs_list`/`sim_fs_sync`/`sim_fs_crash`/
  `sim_fs_snapshot`), and virtual-clock actor clusters (`sim_start`/`sim_stop`/`sim_now`/`sim_advance`/
  `sim_rand`/`sim_fault`/`sim_log`, `sim_actor_spawn`, `sim_actor_send`, `sim_step`, `sim_run_until`,
  `sim_partition`, `sim_actor_state`, `sim_trace`), alongside OTP-style supervised actors (`actor_spawn`/
  `actor_send`/`actor_ask`/`actor_stop`/`actor_status`/`actor_mailbox_len`, `supervisor_spawn` with
  `"one_for_one"`, `"one_for_all"`, `"rest_for_one"`, `supervisor_status`) and location-transparent
  distributed TCP actor RPC (`actor_node_listen`/`actor_node_stop`/`actor_remote_send`/`actor_remote_ask`).
- `dap.rs`, `lsp.rs`, and `server.rs` implement the time-travel reversible DAP server with forward & reverse
  data watchpoints (`stepBack`, `reverseContinue`, `dataBreakpointInfo`, `setDataBreakpoints`),
  the LSP server, and the AST dependency-driven reactive topological DAG notebook server (`veyra notebook [dir] [port]`,
  cycle detection, `GET /api/notebook/dag`, `GET /api/notebook/export`).
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
  lexical upvalues (`Arc<Mutex<Val>>` cells), first-class contracts (`requires`,
  `ensures`) and static transitive `@pure` / `@effect(...)` verification, classes with
  C3 MRO multiple inheritance, `super()`, descriptors (`@property`), metaclasses,
  stacked decorators, and static/runtime type enforcement, arbitrary-precision
  `BigInt`/`BigDecimal`/`Complex`/`FFT`, unwind-safe `try`/`catch`/`finally` and
  `with ... as ...` context managers, generators, comprehensions, full structural
  `match`/`case` pattern matching (literals, `*rest` sequences, `**rest` maps,
  class patterns with positional/keyword fields, `|`, `as`, `if` guards),
  short-circuit `and`/`or`, right-associative `**`, tuple `(a, b)` and set `{a, b}`
  literals, f-strings, compound assignments (`+=`, `-=`, `*=`, `/=`), list/map index
  mutation (`xs[i] = v`, `m[k] = v`), 1/2/3-arg `range(...)`, async functions with
  `await`/`gather`, tasks, channels, timeout-aware waits, deterministic simulation
  (`sim_start`/`sim_stop`/`sim_now`/`sim_advance`/`sim_rand`/`sim_fault`/`sim_log`),
  supervised actors (`actor_spawn`/`actor_send`/`actor_ask`/`actor_stop`/`actor_status`),
  indirect/chained calls (`f(x)(y)`), keyword arguments/`*args`/`**kwargs`, chained
  comparisons (`0 <= x < 10`), lazy generator expressions (`(x * x for x in xs)`),
  class dunders (`__add__`, `__sub__`, `__mul__`, `__eq__`, `__getitem__`,
  `__setitem__`, `__len__`, `__str__`, `__repr__`, `__call__`), expanded Python-parity
  builtins/methods (`repr`, `callable`, `divmod`, `getattr`, `hasattr`, `setattr`,
  `sorted`/`min`/`max` with `key=`, variadic `zip`, `enumerate(..., start=...)`,
  `print(..., sep=..., end=...)`), mark-sweep object GC (`gc_collect`/`gc_stats`/
  `gc_pause`/`gc_resume`), and in-process CPython C-API + PEP 3118 buffer + PyCapsule
  DLPack interop (`py_import`, `py_call`, `py_get`, `py_set`, `py_dir`, `py_eval`,
  `py_exec`, `py_to_tensor`, `tensor_to_dlpack`, `py_from_dlpack`).
- **Bytecode, VM, WASM & Native AOT**: portable `veyra-bc-v1` (`.vybc`) bytecode
  serialization with SHA-256 tamper verification, multi-pass bytecode optimizer
  (`ir.rs`), flat `CallFrame` + indexed local slot VM (`vm.rs`), standalone WebAssembly
  emitter/disassembler/VM (`veyra wasm`), direct zero-toolchain PE32+/ELF64 native
  executable emission (`veyra aot --direct`), pure-Veyra self-hosted compiler
  (`compiler-vy/src/main.vy`) verified to a 3-stage bootstrap fixpoint
  (`Stage 2 .vybc == Stage 3 .vybc`), and full-parity native AOT compilation (`aot.rs`,
  `veyra aot`, `veyra build --native`) including async, generators, dunders, and Python bridge.
- **Runtime services**: JSON, TOML, CSV, regex, Base64, UUID, date/time, filesystem,
  paths, processes, environment, platform introspection, and capability sandboxing
  (`veyra run --caps c1,c2`, `cap_current`, `cap_restrict`, `veyra audit`).
- **Application**: HTTP client, path-traversal-hardened static/routed/dynamic web
  servers, request and response headers, persistent stateful reactive notebook server
  (`veyra notebook [dir] [port]`), SQLite with parameter binding, ORM foundations.
- **Security**: SHA-256, HMAC-SHA256, PBKDF2, constant-time equality, lockfile
  integrity audit with tamper detection, capability enforcement (`--caps`, manifest
  `"capabilities"`, `cap_restrict`), and pure-Rust C header FFI bindgen (`veyra ffi bindgen`).
- **Data/ML**: dataframe transforms and joins, statistics, linear algebra,
  regression, clustering, ML metrics, fused single-pass Vulkan SPIR-V & HLSL GPU
  pipelines (`gpu_fuse`, `gpu_fuse_inspect`), CPython DLPack zero-copy tensor interop
  (`tensor_to_dlpack`, `py_from_dlpack`), plus an N-dimensional tensor runtime with
  NumPy/PyTorch-style N-D broadcasting and backward gradient reduction, cache-friendly
  `tensor_matmul`, reverse-mode autograd with automatic unreachable tape GC
  (`tensor_gc_unreachable`), Adam and SGD optimizers, neural network / transformer
  layers (`tensor_scale`, `tensor_cross_entropy`, `tensor_layer_norm`, `tensor_conv1d`,
  `tensor_conv2d`, `tensor_attention`, `tensor_multihead_attention`), and checkpoint
  save/load serialization.
- **Packages**: offline registry by default, federated registries (`VEYRA_REGISTRY_URLS`)
  with offline disk cache (`VEYRA_OFFLINE=1`), hosted & self-hosted HTTP registry
  (`veyra pkg serve`, `POST /publish`, `.vypkg.json` reproducible bundles), HMAC-SHA256 signing &
  verification, scoped `@org/pkg` + semver constraints, monorepo `"workspaces"`,
  `veyra.lock` format 3, flat and per-platform `veyra_plugin_v1` native binary plugins,
  `veyra pkg py-bridge`, recursive dependencies with cycle protection, and 74 shipped
  packages verified both statically and at runtime by `scripts/package-matrix.ps1`.
- **Tooling**: `run`/`check`/`lint`/`fmt`/`test`/`build`/`aot`/`wasm`/`ffi`/`notebook`/
  `repl`/`disasm`/`bench`/`profile`/`doc`, line-aware LSP diagnostics and navigation
  (completion, hover, definitions, references, rename, hierarchical symbols, formatting),
  time-travel reversible Debug Adapter Protocol server (`veyra dap` with breakpoints,
  stepping, `stepBack`, `reverseContinue`, stack frames, variable inspection, and
  `evaluate`), and the VS Code extension (`tooling/vscode/`).

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
- Sovereign parity extensions (Pillars 1-5): in-process CPython C-API + PEP 3118
  zero-copy tensor buffers (`py_bridge.rs`), universal Vulkan SPIR-V + Direct3D 11 `f64`/`f32`
  GPU compute (`gpu.rs`), multi-arch x86_64/aarch64 + list slicing + inter-call JIT (`jit.rs`),
  `BigInt`/`BigDecimal`/`Complex`/`FFT`, C3 MRO/`super()`/descriptors/metaclasses, full structural
  `match`/`case` pattern matching, upgraded `vydecimal` `0.2.0` + 6 new packages (74 total), and
  federated registry/workspaces/scoped packages/platform native plugins/`veyra pkg py-bridge` (`registry.rs`).

## Repository map

| Path | Contents |
|---|---|
| `engine-rs/` | Native Rust Horizon engine (`lib.rs`, `main.rs`, `builtins/`, `ir.rs`, `vm.rs`, `jit.rs`, `gpu.rs`, `plugin.rs`, `registry.rs`, `aot.rs`, etc.) |
| `compiler-vy/` | Pure-Veyra self-hosted compiler emitting `veyra-bc-v1` (`.vybc`) |
| `bin/` | Built native launcher (`bin/veyra-native.exe`) |
| `packages/` | 74 native Veyra packages |
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
