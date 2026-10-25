> **Historical record.** Superseded by [STATUS.md](STATUS.md) and
> [CHANGELOG.md](CHANGELOG.md). Retained for provenance.

# Veyra 50% Independent Ecosystem Milestone

"50%" here means roughly half of the practical platform categories we previously relied on Python for now have a usable Veyra-owned/native path. It does not mean Veyra has half as many packages, users, maintainers or years of production history as Python.

## Now independent of Python

### Runtime and execution
- Native Windows executable
- Native lexer/parser/AST
- Native bytecode compiler
- Native stack VM
- Native user functions and recursion
- Native constants, control flow, lists, maps and indexing
- Native module loading

### Standard services
- JSON
- regex
- text utilities
- math utilities
- files/directories/paths
- environment access
- subprocesses
- HTTP GET
- random/time
- SHA-256
- SQLite

### Project tooling
- project creation
- manifests
- checking
- formatting
- project test runner
- distribution builds
- bytecode disassembly
- REPL

### Package ecosystem
- Veyra-owned registry
- Veyra package search/list
- add/remove
- vendoring
- lockfiles
- restore/install
- local package publishing
- package-name module resolution
- seven Veyra-native packages currently seeded

### Editor path
The installed VS Code support invokes the default Veyra launcher, which is now the native executable rather than Python.

## Verified independence

A built distribution was run with Python absent from PATH:

    PYTHON_NOT_ON_PATH
    Veyra 4.0.0-sovereign
    PASS native package mean
    PASS native package factorial
    PASS published package
    ECOSYSTEM_OK
    EXIT=0

The package lifecycle was also verified:

    publish -> registry search -> add -> use by package name -> run
    -> delete vendor -> install/restore -> run -> build

## Still not independent enough

Major remaining gaps include:

- native classes/object model
- native exceptions
- captured closures
- general iterators/generators/comprehensions
- async/await and structured concurrency
- a native GUI framework
- web-server framework
- high-performance numerical array library
- plotting/dataframe libraries
- native ML/GPU stack
- direct C ABI/FFI
- production LSP/debugger
- remote hosted public registry and package signing
- Linux/macOS/ARM release validation
- optimizer/JIT/native-code backend
- self-hosted compiler

Those are the next steps for moving independent ecosystem maturity beyond this milestone.
