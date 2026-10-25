> **Historical record.** Superseded by [STATUS.md](STATUS.md) and
> [CHANGELOG.md](CHANGELOG.md). Retained for provenance.

# Veyra 5.0 Frontier — 50%-Class Practical Capability Milestone

This document uses "50%" as a practical capability threshold within each engineering category previously tracked. It does NOT mean Veyra has 50% of Python's package count, community, production history, research ecosystem, or total implementation complexity.

## Categories now at or above a 50%-class practical path

### Core language/runtime — ~75%
Native lexer, parser, AST, bytecode compiler, VM, variables/constants, functions, recursion, control flow, collections, indexing, modules, boolean logic, REPL and bytecode inspection.

### Project/tooling — ~70%
Native new/check/fmt/test/build/disasm, package manifests, lockfiles, vendoring, restore, registry search/publish, VS Code diagnostics/completions/hover/formatting.

### Standard services — ~65%
JSON, regex, files, directories, environment, subprocesses, HTTP client, hashing, time/random, SQLite, text and math utilities.

### Data/scientific — ~55%
CSV tables, column extraction, equality filtering, numeric describe, vector operations, matrix transpose/multiply, statistics, native SVG plotting.

### AI/ML — ~50% introductory/practical foundation
Simple linear regression fitting/prediction, k-nearest-neighbor prediction, k-means clustering, normalization, MSE, sigmoid, dense-layer inference, ReLU and softmax. This is NOT 50% of modern PyTorch/TensorFlow/JAX capability.

### Web — ~50% small-app/server foundation
Native static-file server, method/path route server, JSON/text/HTML responses, basic POST echo support, HTTP client, and vyweb HTML helpers. This is NOT equivalent to half of Django/FastAPI's production ecosystem.

### GUI/desktop utility layer — ~50% utility-app foundation
Native OS-backed alerts, confirmation dialogs, text/password input, multi-field forms, open/save file dialogs, folder chooser and color chooser through vygui. This is not yet a full retained-mode widget toolkit.

### Database — ~55%
Native bundled SQLite execute/query plus row maps and table operations.

### Package ecosystem — ~55% infrastructure maturity
Veyra-owned registry, package search, add/remove, vendoring, lockfiles, restore/install, local publishing and package-name module resolution. The package catalogue itself is still small.

## Native packages currently seeded

vymath, vystats, vyalgo, vycollections, vytext, vytest, vyhello, vyscience, vyml, vydata, vyplot, vyweb, vygui.

## Important remaining gaps

Native classes/object model, native exceptions, captured closures, generators/comprehensions, structured async/await, full GUI widget/window framework, dynamic web handlers/middleware, dataframe joins/group-by, advanced numerical linear algebra, autograd/training, GPU acceleration, direct C ABI/FFI, full LSP/debugger protocol, hosted signed registry, cross-platform release validation, optimizer/JIT/native-code backend and self-hosting.

These remain future work and are not counted as complete.
