# Veyra 90%-Practical Milestone

This milestone uses two separate measurements:

1. Practical capability reach: what a Veyra project can actually do today, including capabilities exposed through the Python bridge.
2. Native implementation depth: what the Rust Horizon engine independently implements without Python.

## Practical capability reach

The following Python-comparison areas are now substantially reachable from Veyra projects:

- General scripting and automation
- Typed functions and recursion
- Object-oriented programming
- Structured exceptions
- Core collections
- Filesystem and environment access
- JSON and regular expressions
- HTTP/network clients
- SQLite and database ecosystem access
- Thread-pool tasks and concurrent mapping
- Scientific/data libraries through py_import
- AI/ML libraries through py_import
- Web frameworks through py_import
- Image/vision libraries through py_import
- Python/C/C++/Rust extension ecosystems indirectly through Python packages
- Isolated project environments
- Dependency add/remove/list/install/lock/audit
- Formatter, linter, semantic checker, tests, benchmark, profiler, trace
- API documentation generation
- REPL
- VS Code syntax, completion, hover, formatting and diagnostics
- Native bytecode VM execution for a growing core
- Native bytecode disassembly
- Native functions and recursion
- Native for-range loops
- Native maps/lists/indexing
- Native const enforcement
- Native modules through use
- Native boolean logic

This is the basis for calling the milestone about 90% of the practical capability checklist used in the Python comparison. It does NOT mean the native Veyra compiler independently contains 90% of CPython, PyPI, NumPy, PyTorch, Django, and decades of ecosystem work.

## Native Horizon remaining major gaps

- Native classes and inheritance
- Native exceptions
- Native closures with captured mutable environments
- Native general iterators/generators/comprehensions
- Native async/await scheduler
- Garbage collector / formal memory model
- Generics and traits/interfaces
- Advanced static inference
- JIT and optimizing native-code backend
- C ABI/FFI implemented directly in Veyra
- Linux/macOS/ARM release verification
- Full debugger protocol and production LSP
- Native package registry
- self-hosted compiler

Those remain roadmap work rather than being mislabeled as complete.
