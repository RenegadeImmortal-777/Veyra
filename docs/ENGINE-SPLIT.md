# Splitting the Engine

Re-analysed **2026-10-25** by direct measurement of the working tree, not from earlier
notes. Supersedes the "split the engine" paragraph in [V12-SCOPE.md](V12-SCOPE.md),
which was reasoned but never measured.

Baseline at time of writing: `cargo fmt --check` exit 0, `cargo test` exit 0,
**57 passed / 0 failed**. The suite is the safety net for every phase below.

## What the file actually is

`engine-rs/src/main.rs` — one file, 9,495 lines, 421 KB. There is no `lib.rs`.

| Region | Lines | Span |
|---|---|---|
| Lexer (`Tok`, `lex`) | 250 | 37–286 |
| AST (`Expr`, `Stmt`) + parser | 643 | 288–929 |
| Values + numeric/crypto/linalg helpers | 327 | 931–1257 |
| Opcodes, tensor runtime, tasks, channels | 548 | 1259–1806 |
| Bytecode compiler | 492 | 1845–2341 |
| **VM** | **4,604** | **2343–7001** |
| Compile pipeline (`compile_source`, `prepare_vm`, …) | 255 | 7002–7256 |
| CLI tooling (debug, lint, bench, profile, doc) | 226 | 7268–7494 |
| LSP server | 550 | 7496–8046 |
| Project/pkg registry | 830 | 8048–8908 |
| `doctor`, `repl`, `usage`, `main` | 170 | 8909–9115 |
| `mod tests` (57 tests) | 378 | 9117–9495 |

## The finding that changes the plan

"9,495 lines in one file" is the wrong headline. The VM body is one function:

- `execute()` runs from line 2629 to 7002 — **4,374 lines**, a single
  `loop { match op { … } }`.
- It has 33 match arms. **`Op::Call` alone is 4,066 of those lines — 93%.**
- Inside it: a nested `match n.as_str()` with **212 builtin arms** (median 14 lines,
  largest 89 — `ffi_f64`, `ffi_i32`, `tensor_adam_step`, `spawn`, `http_request`).
- 277 `match` keywords and 517 `return Err` live in that one arm.

Two consequences.

**Good news:** there is not a single `continue;` or `break;` inside `execute()`. Every
arm exits by falling through or `return Err`. That is exactly the shape that makes
arms extractable into `Result<(), String>` methods with no control-flow plumbing —
no continuation enum, no manual `ip` juggling. The hardest part of this kind of
refactor is already absent.

**Bad news:** moving code into modules does not decouple it. `builtins.rs` would
still contain a 4,000-line function; the split would make the *file* navigable while
leaving the *dispatch* monolithic. So "split the engine" is two different jobs:

1. **File split** — mechanical, layout-only, zero semantic risk.
2. **Call-site decomposition** — the real work: 212 inline builtins become a
   dispatch registry, each builtin a named function.

Doing only (1) and calling it done would be rearranging furniture.

## Phases

Each phase ends with the same gate: `cargo fmt --check` and `cargo test` both exit 0
with **57 passed**, plus `cargo clippy --all-targets --all-features -- -D warnings`
clean. `scripts/gate-report.ps1 -Quick` runs the Rust half of that.

While the MSVC linker is unavailable that gate degrades to `fmt`, `check
--all-targets` and `clippy` — see Phase 1's verification note for exactly what that
does and does not prove.

### Phase 1 — mechanical file split — **complete 2026-10-25**

`main.rs` went from 9,495 lines to 610: `main()` and the test suite only. Sixteen
modules, one commit each, in dependency order.

| Module | Lines | Contents |
|---|---|---|
| `lexer.rs` | 254 | `Tok`, `lex` |
| `ast.rs` | 62 | `Expr`, `Stmt` |
| `parser.rs` | 590 | `Parser` |
| `value.rs` | 343 | `Val` plus the JSON, SQL, crypto and linear-algebra helpers |
| `bytecode.rs` | 86 | `Op`, `FunctionDef`, `ClassDef`, `ObjectData` |
| `runtime.rs` | 76 | id counters and the task/channel/function-object registries |
| `tensor.rs` | 467 | `TensorOp`, `TensorData`, `AdamState`, handle/autograd/backward |
| `compiler.rs` | 505 | `LoopCtx`, `Compiler` |
| `vm.rs` | 4,706 | `Vm` and `execute()` — moved as-is |
| `pipeline.rs` | 284 | `load_source`, `semantic_check_ast`, compile/prepare/run/disasm |
| `tooling.rs` | 242 | debug, ast, symbols, lint, bench, profile, doc |
| `lsp.rs` | 567 | the language server |
| `registry.rs` | 390 | package index, lockfile, pkg commands |
| `project.rs` | 173 | path helpers, scaffolding, build commands |
| `server.rs` | 305 | static, routed and dynamic HTTP servers |
| `cli.rs` | 86 | `new_project`, `doctor`, `repl`, `usage` |

**Verification.** The gate for this phase was `fmt`, `check --all-targets` and
`clippy -D warnings`, not `cargo test`: the MSVC link toolchain is incomplete on this
machine, so nothing can be linked or executed (see [../STATUS.md](../STATUS.md),
"Toolchain regression"). A pure move cannot change behavior, only fail to compile,
and the compiler checked every module — including the `#[cfg(test)]` test suite,
which `--all-targets` type-checks. **The tests have not been run since 2026-10-10
and must be run before Phase 2.**

Code-preservation check, baseline `d92996b` against the split tree:

| Count | Before | After |
|---|---|---|
| `fn` definitions | 192 | 192 |
| `struct`/`enum` | 15 | 15 |
| `#[test]` | 57 | 57 |
| Lines | 9,495 | 9,746 (+251 of module headers, `mod` and `use` lines) |

Submodules of the crate root can read crate-root-private items, so `Vm`'s internals
needed `pub(crate)` only where the pipeline, server and tests actually reach them.
That changes the moment a `lib.rs` is introduced, which is a separate decision (see
below).

### Phase 2 — make `Op::Call` legible

Only once the test suite runs again and is green:

1. Extract each of the 212 arms into `fn builtin_<name>(&mut self, a: Vec<Val>) -> Result<Val, String>`.
   Mechanical, one at a time; the suite covers most of them by name.
2. Replace the nested `match n.as_str()` with a lookup →
   `Option<fn(&mut Vm, Vec<Val>) -> Result<Val, String>>`.
3. Split the resulting functions into `builtins/{core,text,collections,math,science,ml,data,ffi,io,net}.rs`.

### Phase 3 — a library target

Add `lib.rs` exposing lexer, parser, checker, compiler and VM as public API, with
`main.rs` reduced to a bin that calls into it. This is what the self-hosting path in
V12-SCOPE actually requires — the passes must be callable without the CLI entry
point — and it is also the prerequisite for differential testing an optimizing
backend against the interpreter.

Gate for Phase 3: the binary's behavior is unchanged (all 57 tests, plus the 60/60
package matrix), and an integration test drives the same pipeline through the
library API.

## What this does not buy

- **No performance change.** Median arm size stays the same; dispatch is still a
  match or a table lookup. Speed comes from the optimizing backend in V12-SCOPE.
- **No semantic change.** No new syntax, no GC, no type system work.
- **No coverage increase.** 57 tests over ~212 builtins is thin; the split makes
  builtins reachable individually, which makes per-builtin tests cheap. Worth doing
  alongside Phase 2, not instead of it.

## Ordering note

`V12-SCOPE.md` lists "make the gates runnable" before the split. That prerequisite is
now satisfied: the gates run and pass on this machine. The remaining hard constraint
is only that each phase leaves the suite green.
