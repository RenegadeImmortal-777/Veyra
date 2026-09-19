# Veyra Horizon

Veyra is now a dual-engine programming-language platform for Windows.

## Engines

- Compatibility engine: Veyra 1.0 interpreter with functions, recursion, loops, lists, maps, modules, runtime types, REPL and built-ins.
- Horizon engine: standalone Rust executable with lexer, parser, AST, bytecode compiler and stack virtual machine.

## Commands

    veyra --version
    veyra doctor
    veyra new myapp
    veyra run [file.vy]
    veyra check [file.vy]
    veyra fmt [file.vy]
    veyra test
    veyra build
    veyra clean
    veyra native run file.vy
    veyra native doctor
    veyra info

Opening a .vy file in VS Code enables the locally installed Veyra language extension.

## Repository layout

- veyra.py: compatibility runtime
- veyra_cli.py: unified Horizon CLI
- engine-rs/: native Rust compiler/VM
- bin/: native release executable
- stdlib/: standard-library seed
- examples/: compatibility and native examples
- tests/: regression tests
- spec/: architecture and language specification
- tooling/vscode/: VS Code language package
- ROADMAP.md: native-parity and long-horizon research targets

## Current guarantee

The compatibility test suite, native Rust test suite, sample programs, project creation, checking, building and doctor checks all pass on the development machine.
