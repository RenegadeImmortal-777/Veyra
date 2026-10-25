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

### Phase 1 — mechanical file split (no behavior change)

Extract in dependency order, test after each module:

1. `lexer.rs` — `Tok`, `lex` — **done 2026-10-25**, verified by `cargo fmt --check`,
   `cargo check --all-targets --all-features` and `clippy -D warnings` only. The test
   suite could not run: the MSVC link toolchain on this machine is incomplete (see
   [../STATUS.md](../STATUS.md), "Toolchain regression"). `main.rs` 9,495 → 9,248
   lines, `lexer.rs` 254 lines.
2. `ast.rs` — `Expr`, `Stmt`
3. `parser.rs` — `Parser`
4. `value.rs` — `Val`, `val_to_json`, `as_matrix`, `hmac_*`, matrix helpers
5. `bytecode.rs` — `Op`, `FunctionDef`, `ClassDef`, `ObjectData`, statics
6. `compiler.rs` — `LoopCtx`, `Compiler`
7. `tensor.rs` — `TensorOp`, `TensorData`, `AdamState`, `tensor_*`
8. `vm/mod.rs` — `Vm` struct, `new`/`nested`/`pop`/`bin_num`/`call_def`/`invoke`
9. `vm/execute.rs` — `execute()` **as-is**. Do not touch it in this phase.
10. `check.rs` — `semantic_check_ast`
11. `tooling.rs` — debug/ast/symbols/lint/bench/profile/doc
12. `lsp.rs` — the LSP server
13. `registry.rs` — pkg index/add/remove/list/install/search/publish/audit
14. `server.rs` — `serve_*`, routing, `query_params`
15. `cli.rs` — `doctor`, `repl`, `usage`, `main`

Submodules of the crate root may read crate-root-private fields, so `Vm`'s internals
do not need to become `pub(crate)` for steps 8–9 in a binary-only crate. That changes
the moment a `lib.rs` is introduced, which is a separate decision (see below).

### Phase 2 — make `Op::Call` legible

Only after Phase 1 is green and committed:

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
