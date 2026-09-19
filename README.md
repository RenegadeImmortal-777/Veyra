# Veyra 4.0 Sovereign

Veyra is now native-first. The default `veyra` command executes the standalone Rust compiler/bytecode VM and does not require Python.

## Native language platform

Veyra Sovereign includes its own lexer, parser, AST, bytecode compiler and stack VM with variables, constants, typed function syntax, user functions, recursion, if/else, while, native for/range loops, lists, maps, indexing, modules, boolean logic, strings and core operators.

## Native standard services

The standalone executable directly provides:

- JSON parse/stringify
- regular expressions
- text case/trim/split/join/contains
- file read/write
- directory listing/creation and file removal
- paths and environment variables
- subprocess execution
- HTTP GET over Rustls
- time and random numbers
- SHA-256 hashing
- SQLite execute/query using bundled SQLite
- core math including sqrt/abs/floor/ceil

These services execute inside Veyra's Rust runtime rather than through Python.

## Veyra package ecosystem

Veyra has its own registry under `registry/index.json`, its own package store, vendoring, manifest and lock file.

Commands:

    veyra registry
    veyra add vymath
    veyra remove vymath
    veyra packages
    veyra install
    veyra pkg search math
    veyra publish

Packages can be imported by name:

    use "vymath";
    use "vystats";

Current native packages include vymath, vystats, vyalgo, vycollections, vytext, vytest and vyhello.

## Native project workflow

    veyra new myapp
    cd myapp
    veyra add vymath
    veyra check
    veyra run
    veyra test
    veyra build

`veyra build` creates a distributable directory containing the standalone `veyra.exe`, project source, manifest and vendored Veyra packages.

## Independence proof

A packaged Veyra application was executed successfully after Python was removed from PATH. The bundled `veyra.exe` loaded vendored Veyra packages, executed math/statistics package code and completed with exit code 0.

## Compatibility bridge

The earlier Python-backed Convergence runtime is preserved as an optional compatibility route. It is no longer the default launcher. This keeps access to Python interoperability while allowing normal Veyra programs and packages to depend on Veyra itself.

## Repository layout

- `engine-rs/` - native Rust compiler, VM, standard services and package CLI
- `bin/veyra-native.exe` - standalone Windows runtime
- `packages/` - Veyra-native package sources
- `registry/` - Veyra package registry
- `veyra.bat` - native-first repository launcher
- `veyra-convergence.bat` - compatibility launcher
- `veyra.py`, `veyra_cli.py` - optional Convergence compatibility stack
- `tooling/vscode/` - Veyra editor integration
- `spec/` - language/architecture documentation
- `STATUS-50-INDEPENDENT.md` - independence milestone definition

## Version

Native runtime: Veyra 4.0.0-sovereign
