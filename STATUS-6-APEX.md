# Veyra 6.0 Apex

Veyra 6.0 Apex advances the native Rust compiler/bytecode VM beyond Frontier with a substantially deeper language and application platform.

## Language core

- Native classes, inheritance, constructors, fields, bound methods and reference-semantic objects.
- Runtime-enforced type annotations for variables, assignments, parameters, return values and user-defined class types.
- Lexical semantic checking for undefined names and duplicate declarations.
- Lexical captures for nested functions.
- try/catch/throw exceptions.
- General iteration over lists and strings.
- break and continue.
- Native await expression over Veyra tasks.
- Constants, functions, recursion, collections, indexing and modules retained from Frontier.
- Everyday helpers: int, float, bool, assert, sum, min, max, all, any, tuple, set, contains, index_of, sorting/slicing/zip/enumerate and functional map/filter/reduce.

## Concurrency and native interop

- Native background tasks with spawn/task_done/await_task.
- Message-passing channels with channel/send/recv/try_recv.
- Direct C ABI dynamic-library calls for i32, i64 and f64 signatures.
- Runtime CPU-count and sleep primitives.

## Web and services

- Static serving and declarative route serving.
- Dynamic Veyra web applications: HTTP routes can invoke functions implemented in .vy files.
- Stateful objects can persist across requests.
- Native HTTP GET and POST.
- SQLite, JSON, regex, files, environment, subprocess, SHA-256 and GUI services.

## Data, science and ML

- Vectors and matrices.
- Mean, variance, standard deviation, median, percentile, covariance and correlation.
- CSV and row-map tables.
- Table column/filter/describe/group/sort/select/inner-join.
- SVG plotting.
- Linear regression, logistic regression training/inference, k-NN, k-means, normalization, dense inference, ReLU, sigmoid, softmax and MSE.

## Toolchain

- new, run, check, fmt, test, build, disasm and repl.
- lint, bench, profile and doc.
- registry search/add/remove/install/publish.
- Content-hashed lockfiles and package-integrity audit.
- VS Code diagnostics, formatting, completions, hover and syntax highlighting.
- Standalone native distribution remains independent of Python.

## Native package catalogue

Frontier packages plus vyhttp, vyconcurrent, vyffi, vyjson, vyregex, vycrypto, vysqlite and vyos.

## Verification

The Apex native Rust regression suite contains 30 tests covering legacy runtime behavior and new classes, inheritance, exception handling, lexical captures, type enforcement, FFI, tasks, channels, await syntax, functional collections, science/data/ML and other services.

Additional release gates exercise the dynamic web server, package-integrity tamper detection, optimized release build and no-Python packaged execution.

## Still beyond Apex

Apex does not claim parity with the entire Python ecosystem. Major future depth includes generators/yield, comprehensions as syntax, pattern matching, richer callable/lambda semantics, descriptors/metaprogramming, a full retained GUI framework, dataframe breadth, advanced numerical linear algebra, autograd/GPU computing, a native optimizing/JIT backend, full LSP/debugger protocols, cross-platform release certification, a hosted signed registry at ecosystem scale and eventual self-hosting.
