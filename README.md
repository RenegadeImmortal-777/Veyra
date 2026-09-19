# Veyra 3.0 Convergence

Veyra is a dual-engine programming language platform with a broad compatibility runtime and a native Rust bytecode VM.

## Language
Veyra supports variables/constants, typed functions, recursion, closures, classes and bound methods, lists/maps/tuples/sets, indexing and member access, modules, if/else, while, for/in, break/continue, try/catch/throw, comments, a REPL, and runtime type enforcement.

## Python ecosystem bridge
A Veyra program can import project-installed Python modules with:

    let np = py_import("numpy");

Member access and calls work directly:

    let math = py_import("math");
    print(math.sqrt(81));

Keyword arguments are available through:

    py_call(fn, [arg1, arg2], {"name": value});

Project dependencies are isolated under .veyra/venv.

## CLI
    veyra new app
    veyra run [file.vy]
    veyra check [file.vy]
    veyra fmt [file.vy]
    veyra lint [file.vy]
    veyra test
    veyra bench [file.vy] [iterations]
    veyra build
    veyra add <python-package>
    veyra remove <python-package>
    veyra packages
    veyra lock
    veyra install
    veyra audit
    veyra env
    veyra native ...
    veyra doctor
    veyra info

## Runtime services
Veyra exposes JSON, regex, files, environment access, HTTP GET, SQLite, subprocesses, random utilities, parallel_map and Python reflection helpers.

## Architecture
- veyra.py: Convergence compatibility runtime
- veyra_cli.py: unified CLI and project environment manager
- engine-rs/: native Rust lexer/parser/AST/bytecode compiler/VM
- bin/veyra-native.exe: optimized native VM
- stdlib/: Veyra standard-library seed
- tooling/vscode/: local VS Code language support
- spec/: architecture and language specification
- tests/: regression suite

The Python bridge is an interoperability strategy, not a claim that Veyra's native VM independently reimplements Python's ecosystem.

## 90%-Practical milestone

See STATUS-90.md for the exact distinction between practical capability reach and native implementation depth. Veyra can now reach much of the Python ecosystem while Horizon continues replacing fallback paths with native compiler/VM features.
