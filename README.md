# Veyra 10.0 Zenith

Veyra is a native-first general-purpose language and application platform with its own Rust lexer, parser, semantic checker, bytecode compiler, VM, package ecosystem and developer tooling.

## Zenith highlights

- typed OOP, inheritance, closures, escaping function objects
- generators, comprehensions and match/case
- decorators, with/as, try/catch/finally
- async functions, await, gather, tasks, channels, task status and timeout-aware waits
- module objects via import_package()
- C ABI FFI
- dynamic web applications and native HTTP services
- parameterized SQLite and ORM foundations
- dataframe/statistics/plotting foundations
- trainable tensors with reverse-mode autograd
- differentiable sigmoid/tanh/exp/log plus reshape and transpose
- tensor/model serialization without Python
- gradient descent optimizers
- HMAC/PBKDF2 security primitives and package integrity auditing
- LSP completion/hover/definition/references/rename/symbols/formatting
- VS Code Check/Run/Debug/Test/Profile commands
- cross-platform CI and tagged release artifact workflow
- 52 Veyra-native packages

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

See STATUS-10-ZENITH.md, STATUS-9-ASCENDANT.md and docs/PACKAGES.md.
