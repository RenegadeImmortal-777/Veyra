# Veyra 11.0 Nexus

Veyra is a native-first general-purpose language and application platform with its own Rust lexer, parser, semantic checker, bytecode compiler, VM, package ecosystem and developer tooling.

## Nexus highlights

- typed OOP, inheritance, closures, escaping function objects
- generators, comprehensions and match/case
- decorators, with/as, try/catch/finally
- async functions, gather, tasks, channels, task status and timeout-aware waits
- module objects via import_package()
- C ABI FFI
- dynamic web applications with request headers and arbitrary response headers
- parameterized SQLite and ORM/query-building foundations
- dataframe/statistics/plotting/ETL foundations
- trainable tensors with reverse-mode autograd
- differentiable division, negation, power, sigmoid/tanh/exp/log, reshape and transpose
- row-wise softmax, detach/numel helpers, model serialization and Adam optimization
- HMAC/PBKDF2 security primitives and package integrity auditing
- local/offline registry plus hosted HTTP registry support
- recursive package dependencies with cycle protection
- LSP completion/hover/definition/references/rename/symbols/formatting
- VS Code Check/Run/Debug/Test/Profile commands
- cross-platform CI and tagged release artifact workflow
- 60 Veyra-native packages

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
