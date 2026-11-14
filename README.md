# Veyra 12.0 Sovereign

Veyra is a native-first general-purpose language and application platform with its own Rust lexer, parser, semantic checker, bytecode compiler, VM, multi-arch native JIT, universal GPU compute, native AOT compiler, federated package ecosystem, and developer tooling.

## Sovereign highlights

- full Python-parity OOP: C3 MRO multiple inheritance, `super()`, descriptors (`@property`), metaclasses, stacked decorators, closures, escaping function objects, and arbitrary-precision `BigInt`/`BigDecimal`/`Complex`/`FFT`
- full structural `match`/`case` pattern matching (literals, sequences with `*rest`, mappings with `**rest`, class patterns with positional/keyword fields, `|`, `as`, `if` guards), generators, comprehensions
- decorators, `with`/`as`, `try`/`catch`/`finally`
- async functions, `gather`, tasks, channels, task status and timeout-aware waits
- module objects via `import_package()`
- C ABI FFI + pure-Rust C header `struct`/`enum`/`#define` bindgen (`veyra ffi bindgen <header.h> <lib_path>`) + dynamic multi-arg x86_64 C-ABI calls + bounded `mem_*` buffers + in-process CPython C-API + PEP 3118 & strided PyCapsule DLPack (`tensor_to_dlpack` / `py_from_dlpack`) zero-copy tensor interop
- IR SSA / algebraic / DCE optimizer (`ir_stats`), compile-time escape-analysis `auto_arena`, self-hosted `compiler-vy` arithmetic constant-folding peephole, first-class `requires` / `ensures` contracts, transitive `@pure` / `@effect(...)`, formal `@secret` / `sanitize(...)` taint flow, `@linear` / `linear_consume(...)` ownership across branches/actors/remotes, and relational difference-bound `@verify` proofs with counterexample synthesis
- standalone WebAssembly/WASI emitter with linear-memory strings & arrays (`veyra wasm`), direct zero-toolchain PE32+/ELF64 emission with strings & arrays (`veyra aot --direct`), and multi-arch x86_64/aarch64 native JIT (dynamic `[..]` lists, `Op::SetIndex`, `log2`/`hypot`/`fmod`/`atan2`)
- rich `gpu_devices` inspection, shared-memory tiled GEMM, causal online FlashAttention, fused Residual+LayerNorm+activation & fused GEMM pipelines (`SPIR-V` + `HLSL`, `gpu_kernel_inspect`, `gpu_fuse`, `gpu_fused_linear`), `f16`/`bf16` casting, 2-generation nursery/old-gen GC + `arena_scope(fn)`/`auto_arena`, flight-recorder profiler (`profile_start`/`profile_stop`/`profile_report`, `veyra profile`), deterministic crash-safe VFS & virtual-clock actor clusters (`sim_fs_*`, `sim_actor_*`, `sim_partition`, `sim_trace`), OTP supervision trees (`supervisor_spawn`), and location-transparent TCP actor RPC (`actor_node_listen`, `actor_remote_send`, `actor_remote_ask`)
- capability sandboxing (`veyra run --caps`, `cap_current`, `cap_restrict`, `veyra audit`), reproducible `.vypkg.json` bundles, reversible time-travel DAP with forward/reverse data watchpoints (`stepBack`, `reverseContinue`, `setDataBreakpoints`), and reactive topological DAG notebooks (`veyra notebook`, `/api/notebook/dag`, `/api/notebook/export`)
- dynamic web applications with request headers and arbitrary response headers
- parameterized SQLite and ORM/query-building foundations
- dataframe/statistics/plotting/ETL foundations
- trainable tensors with reverse-mode autograd
- differentiable division, negation, power, sigmoid/tanh/exp/log, reshape and transpose
- row-wise softmax, detach/numel helpers, model serialization and Adam optimization
- HMAC/PBKDF2 security primitives and package integrity auditing
- federated/local/offline registry (`VEYRA_REGISTRY_URLS`, `VEYRA_OFFLINE=1`), scoped `@org/pkg` packages, monorepo `"workspaces"`, per-platform `"native"` artifacts, and recursive dependency resolution with cycle protection
- LSP completion/hover/definition/references/rename/symbols/formatting and DAP debugger
- VS Code Check/Run/Debug/Test/Profile commands
- cross-platform CI and tagged release artifact workflow
- 74 Veyra-native packages (including `vyjit`, `vygpu`, `vyplugin`, `vycache`, `vydiff`, `vytemplate`, `vyargparse`, `vydecimal` `0.2.0`, `vycomplex`, `vyitertools`, `vyfunctools`, `vytyping`, `vygraph`, `vysemver`)
- Multi-architecture x86_64 + aarch64 native JIT compiler (`jit.rs`, with list indexing/slicing and inter-call compilation) and universal Vulkan SPIR-V + Direct3D 11 (`f64`/`f32`) GPU compute backend (`gpu.rs`)
- Sovereign HMAC-SHA256 signed registry, `.vypkg` bundles, semver resolution, HTTP registry server (`veyra pkg serve`), and `veyra_plugin_v1` native CDylib extension ABI (`plugin.rs`)

Example:

    let tensor = import_package("vytensor");
    let autograd = import_package("vyautograd");
    let optim = import_package("vyoptim");

    let w = tensor.create([[0]], true);
    let x = tensor.create([[1],[2],[3],[4]], false);
    let y = tensor.create([[2],[4],[6],[8]], false);

    let epoch = 0;
    while epoch < 40 {
        let pred = tensor.matmul(x, w);
        let diff = tensor.sub(pred, y);
        let loss = tensor.mean(tensor.mul(diff, diff));
        autograd.backward(loss);
        optim.step(w, 0.05);
        optim.zero_grad(w);
        epoch = epoch + 1;
    }

The default execution path does not require Python.

See [STATUS.md](STATUS.md) for the current release state and known gaps,
[CHANGELOG.md](CHANGELOG.md) for release history, and docs/REMOTE-REGISTRY.md and
docs/PACKAGES.md for package details.
