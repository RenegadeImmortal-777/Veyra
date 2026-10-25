# Veyra Status

Single authoritative statement of where this project currently stands. If this file
and any other document disagree, this file and [CHANGELOG.md](CHANGELOG.md) win. The
older `STATUS-*.md` and `PYTHON-*.md` files are historical records, retained for
provenance and marked as such.

## Current release

**Veyra 11.0 "Nexus"** — the native Rust engine, last recorded release gate on
2026-09-21.

| Where | Value |
|---|---|
| `engine-rs/src/main.rs` (`VERSION` const) | `11.0.0-nexus` |
| `engine-rs/Cargo.toml` | `11.0.0` |
| Native launcher | `bin/veyra-native.exe`, invoked by `veyra.bat` |
| VS Code extension | `tooling/veyra-language-11.0.0.vsix` |
| Legacy Python runtime | `3.2.0-convergence` (`veyra.py`, `veyra_cli.py`) |

## Two execution paths

This repository contains two runtimes. Both are real; confusing them is the easiest
way to misread the project.

1. **Native Horizon engine** — Rust lexer → parser → AST → bytecode compiler → stack
   VM, in `engine-rs/`. This is the default execution path and does not require
   Python. All current release claims refer to this engine.
2. **Convergence runtime** — the Python compatibility layer. `veyra_cli.py` imports
   `veyra` explicitly as `legacy` and delegates native commands to
   `bin/veyra-native.exe`. This path is what exposes `py_import` and the wider
   Python package ecosystem.

Note the wording in `veyra.bat` versus `INSTALL.bat`: the batch launcher runs the
native engine, but `INSTALL.bat` installs the legacy Python CLI as the `veyra`
command. See Known inconsistencies.

## Verification status

Last full gate run: **2026-10-10**. Every gate passed.

| Gate | Result |
|---|---|
| `cargo fmt --check` | PASS |
| `cargo test` | PASS — 57 passed, 0 failed |
| `cargo check --no-default-features` | PASS |
| `cargo clippy --all-targets --all-features -- -D warnings` | PASS |
| `cargo build --release` | PASS |
| `scripts/package-matrix.ps1` | PASS — 60/60 packages |
| `scripts/ecosystem-gate.ps1` | PASS |
| `scripts/release-check.ps1` | PASS |

Environment of that run: Windows, `cargo`/`rustc` 1.93.1, git `d92996b` on `master`.
Raw output and per-gate exit codes were captured to `gate-report.txt`, which is
gitignored — regenerate it rather than treating this table as permanent.

This independently reproduces the Nexus release claims of 57/57 tests and 60/60
packages on real hardware. It does **not** extend to the installer and extension
version checks listed in the original Nexus record, and it does not cover Linux,
macOS or ARM, which remain CI-build-only.

Confirmed separately by inspection: 60 directories under `packages/`, the engine
source, `bin/veyra-native.exe`, the 11.0.0 extension bundle, and build output
directories for `debug`, `release`, `aarch64-pc-windows-msvc` and
`x86_64-unknown-linux-gnu`.

The engine source is no longer one file. As of 2026-10-25 `engine-rs/src/main.rs` is
610 lines — `main()` and the tests — with the rest in sixteen modules; see
[docs/ENGINE-SPLIT.md](docs/ENGINE-SPLIT.md) for the map and for what that change
did and did not verify.

### Toolchain regression (2026-10-25)

The gates can no longer be run on this machine. The MSVC link toolchain is
incomplete: `Microsoft Visual Studio/18/Community/VC/Tools/MSVC/14.51.36231`
contains `include` but no `bin` or `lib`, `Windows Kits/10/Lib` is empty, and
`vswhere -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64` reports no
installation. `link.exe` on `PATH` resolves to Git's GNU `link`, which fails with
`missing operand`.

What still works, and was run on 2026-10-25 at commit `b187e32`:

| Gate | Result |
|---|---|
| `cargo fmt --check` | PASS |
| `cargo check --all-targets --all-features` | PASS |
| `cargo clippy --all-targets --all-features -- -D warnings` | PASS |

What cannot run until the toolchain is repaired: `cargo test`, `cargo build`,
`cargo build --release`, and therefore `package-matrix.ps1`, `ecosystem-gate.ps1`
and `release-check.ps1`. The existing `target/release/veyra.exe` still runs, but it
was linked on 2026-09-21 and does not include any change made since.

This does not invalidate the 2026-10-10 table: those gates genuinely passed. It does
mean the test suite is currently unavailable as a safety net, which is a hard
constraint on any refactor until it is fixed.

### Re-running the gates

From the repository root:

```
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\gate-report.ps1
```

This runs every gate above in order, preserves each exit code instead of hiding it
behind a pipe, writes `gate-report.txt`, and exits non-zero if any gate fails.

Two things to know when reading the output. The native tests live in the `mod tests`
block at the end of `engine-rs/src/main.rs`, not in a separate `tests/` directory —
the root-level `tests/test_veyra.py` exercises the *Python* runtime, not the native
one. And `release-check.ps1` re-runs the format/test/clippy sequence, so those gates
appear twice in the report; that is expected.

## What the platform does

Native, without Python:

- Language: typed functions and recursion, closures and escaping function objects,
  classes with inheritance and runtime type enforcement, exceptions,
  `with`/`as`, generators, decorators, comprehensions, `match`/`case`, async
  functions with `await`/`gather`, tasks, channels, timeout-aware waits.
- Runtime services: JSON, TOML, CSV, regex, Base64, UUID, date/time, filesystem,
  paths, processes, environment, platform introspection.
- Application: HTTP client, static/routed/dynamic web servers, request and response
  headers, SQLite with parameter binding, ORM foundations.
- Security: SHA-256, HMAC-SHA256, PBKDF2, constant-time equality, lockfile integrity
  audit with tamper detection.
- Data/ML: dataframe transforms and joins, statistics, linear algebra, regression,
  clustering, ML metrics, plus an N-dimensional tensor runtime with reverse-mode
  autograd, Adam and SGD optimizers, and serialization.
- Packages: an offline registry by default, hosted registry over HTTP, recursive
  dependencies with cycle protection, 60 shipped packages.
- Tooling: `run`/`check`/`lint`/`fmt`/`test`/`build`/`repl`/`disasm`,
  `bench`/`profile`/`doc`, an LSP (completion, hover, definitions, references,
  rename, symbols, formatting, diagnostics), and the VS Code extension.

## How to read the percentage milestones

Every `STATUS-*` and `PYTHON-*.md` file in this repository argues for a percentage.
Two distinct things are being measured, and they are not interchangeable:

- **Engineered technical capability** — the compiler, runtime, tooling and libraries
  this repository owns. Scored in the high 70s to low 80s as of Ascendant.
- **Whole-ecosystem parity** — PyPI package count, users, maintainers, production
  history, documentation volume, research adoption, commercial support. This
  dimension is still tiny relative to Python.

The documents consistently exclude ecosystem dimensions from their technical scores
and report them separately. Keep it that way. A local compiler build cannot create
years of production history, and a seed registry of 60 packages is not an ecosystem.

## Known inconsistencies

- `INSTALL.bat` installs `veyra.py` (the legacy 3.2.0 Convergence CLI) as the
  user-level `veyra` command, while `veyra.bat` and the README describe the native
  engine as the default. A fresh install therefore does not match the documented
  default path.
- `examples/` contains `apex`, `ascendant` and `elevation` sample sets but no
  `zenith` or `nexus` directories, so the examples stop four releases behind.
- The native engine is a single 9,495-line file (`engine-rs/src/main.rs`) containing
  lexer, parser, compiler, VM, LSP, registry, web server and tensor stack. This is a
  structural obstacle to the remaining roadmap work.
- `stdlib/` contains only `math.vy` and `testing.vy`; most standard-library behavior
  is built into the engine rather than written in Veyra.
- `spec/ARCHITECTURE.md` and `spec/LANGUAGE-2026.md` had drifted from the
  native-first reality. They are corrected to match this document.

## Remaining gaps

Carried forward from the milestone documents and not claimed as done:

- Self-hosted compiler.
- Optimizing JIT/AOT backend, SIMD specialization.
- Full debugger protocol (DAP) and workspace-wide refactoring.
- Certified Linux/macOS/ARM release artifacts on physical hardware.
- Hosted, signed public package registry at ecosystem scale.
- Columnar/vectorized dataframe engine; NumPy/SciPy-class numerical depth.
- GPU backend, convolution/transformer layers, broadcast/autograd tensor depth.
- Production auth, sessions, WebSockets and middleware depth.
- Rich generics, type inference and protocols; richer object protocol.
- Full event-loop async I/O with structured cancellation.
- A large independent external ecosystem.

## Repository map

| Path | Contents |
|---|---|
| `engine-rs/` | Native Rust engine (single crate, single source file) |
| `bin/` | Built native launcher |
| `packages/` | 60 native Veyra packages |
| `registry/` | Registry index |
| `stdlib/` | Veyra-written standard library seed modules |
| `spec/` | Architecture and language specification |
| `docs/` | Language guide, package index, remote registry notes |
| `examples/` | Per-release example programs |
| `scripts/` | Release, package-matrix and ecosystem gates |
| `tooling/` | VS Code extension bundles per release |
| `tests/` | Python-runtime test suite |
| `veyra.py`, `veyra_cli.py` | Legacy Convergence runtime and CLI |
| `veyra.bat` | Native launcher shim |
| `CHANGELOG.md` | Consolidated release history |
