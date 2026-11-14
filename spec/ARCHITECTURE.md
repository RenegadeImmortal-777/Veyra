# Kovrek Architecture

## Status

This document describes Kovrek 11.0 "Nexus". See [../STATUS.md](../STATUS.md) for the
authoritative current state and [../CHANGELOG.md](../CHANGELOG.md) for history.

## Primary path: the native Horizon engine

`engine-rs/` is a single Rust crate implementing the whole pipeline:

```
source (.kvr)
  -> lexer
  -> parser
  -> AST
  -> semantic checker
  -> bytecode compiler
  -> stack VM
```

The VM provides functions and recursion, closures and escaping function objects,
typed classes with inheritance, exceptions, generators, decorators, comprehensions,
`match`/`case`, async functions with `await`/`gather`, tasks, channels, and direct
C ABI calls through `libloading`.

Beyond the VM, the same crate carries the surrounding platform: the LSP, the package
registry and resolver, the HTTP client and server, the SQLite bindings, the security
primitives, and the tensor/autograd runtime. The binary is shipped as
`bin/kovrek-native.exe` and invoked by `kovrek.bat`.

**This path does not require Python.**

### Modular crate layout

Phase 1 of the engine split moved the lexer, parser, AST, value helpers, bytecode
definitions, shared runtime registries, tensor stack, compiler, VM, compile pipeline,
CLI tooling, LSP, package registry, project helpers, HTTP servers and CLI entry
points into sixteen focused modules under `engine-rs/src/` (see
[../docs/ENGINE-SPLIT.md](../docs/ENGINE-SPLIT.md)). Phase 2 decomposes the inline
builtins inside `vm.rs`, and Phase 3 exposes a library target (`lib.rs`) for the
optimizing backend and self-hosting work.

## Secondary path: the Convergence runtime

The Python compatibility layer is retained and is still functional:

- `kovrek.py` — the Python implementation of Kovrek 3.2.0 "Convergence".
- `kovrek_cli.py` — its CLI, which imports `kovrek` as `legacy` and delegates native
  commands to `bin/kovrek-native.exe`.

This path exists for two reasons: backwards compatibility with the original Kovrek 1
language surface, and Python interoperability through `py_import` / `py_call`, which
the native engine does not provide. Python dependencies are isolated per project
under `.kovrek/venv` and managed by the CLI.

As of Nexus the native engine has the broader language surface; the Convergence
runtime's unique contribution is the Python bridge, not language coverage. The
migration target was always feature parity in the native engine, and that has
largely been reached.

## Tooling

- **CLI:** `run`, `check`, `lint`, `fmt`, `test`, `build`, `repl`, `disasm`, `ast`,
  `symbols`, `debug`, `bench`, `profile`, `doc`, `serve`, `web`, `app`, `lsp`, and
  registry commands.
- **LSP:** completion, hover, definitions, references, rename, document symbols,
  formatting, diagnostics.
- **Editor:** `tooling/kovrek-language-<version>.vsix` bundles, with Check, Run,
  Debug, Test and Profile commands.
- **Packaging:** offline registry by default, hosted registry over HTTP via
  `KOVREK_REGISTRY_URL`, recursive dependencies with cycle protection, content-hashed
  lockfiles and a tamper-detecting integrity audit.

## Release gates

Defined in `scripts/` and exercised by `.github/workflows/`:

| Gate | Script |
|---|---|
| Format, test, portable-core check, clippy, release build | `scripts/release-check.ps1`, `scripts/release-check.sh` |
| Every registry package installs, imports and checks independently | `scripts/package-matrix.ps1`, `scripts/package-matrix.sh` |
| Hosted registry, recursive dependencies, HTTP headers, lockfile audit | `scripts/ecosystem-gate.ps1` |

Portable-core builds (`--no-default-features`) exist so the engine's non-service
core can be checked without native service dependencies. Cross-platform CI covers
Windows, Ubuntu and macOS; this establishes build readiness, not certification on
physical hardware.

## Deployment shape

Windows is the verified platform. Linux, macOS and ARM are covered by CI builds and
cross-compile output directories, but certified release artifacts for those targets
remain roadmap work.
