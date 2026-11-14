# Veyra 12.0 Sovereign

Veyra is a native-first general-purpose language and application platform with its own Rust lexer, parser, semantic checker, bytecode compiler, VM, multi-arch native JIT, universal GPU compute, native AOT compiler, federated package ecosystem, and developer tooling.

## Sovereign highlights

- full Python-parity OOP: C3 MRO multiple inheritance, `super()`, descriptors (`@property`), metaclasses, stacked decorators, closures, escaping function objects, and arbitrary-precision `BigInt`/`BigDecimal`/`Complex`/`FFT`
- full structural `match`/`case` pattern matching (literals, sequences with `*rest`, mappings with `**rest`, class patterns with positional/keyword fields, `|`, `as`, `if` guards), generators, comprehensions
- decorators, `with`/`as`, `try`/`catch`/`finally`
- async functions, `gather`, tasks, channels, task status and timeout-aware waits
- module objects via `import_package()`
- C ABI FFI + in-process CPython C-API (`python3xx.dll`/`libpython3`) + PEP 3118 zero-copy `tensor` buffers + `veyra pkg py-bridge`
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
