# Contributing to Veyra

Thank you for checking out **Veyra 12.0 "Sovereign"**. All core compiler, VM, JIT, GPU, Formal Verification, Actor, and CLI systems live in Rust (`engine-rs/`) alongside the pure-Veyra self-hosted compiler (`compiler-vy/`), 74 curated packages (`packages/`), and VS Code tooling (`tooling/vscode/`).

## Prerequisites

- **Rust toolchain** (`rustup` stable with `rustfmt` and `clippy`; verified on `1.93.1`)
- **Python 3.12+** (only needed for optional `py_bridge` CPython interop tests and legacy compatibility checks)

## Running the Verification Suite

Run the full 8-gate check before opening a pull request:

```powershell
# Windows (runs fmt, cargo test, portable core check, clippy -D warnings, release build, 74 packages, ecosystem & release sync)
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\gate-report.ps1
```

Or run individual checks inside `engine-rs/`:

```bash
cd engine-rs
cargo fmt --check
cargo test
cargo check --no-default-features
cargo clippy --all-targets --all-features -- -D warnings
```

And validate the package matrix on POSIX:

```bash
cargo build --manifest-path engine-rs/Cargo.toml --release
sh ./scripts/package-matrix.sh
```

## Architecture Map

| Directory / File | Purpose |
|---|---|
| `engine-rs/src/pipeline.rs` | Lexer/parser bridge, type/effect checker, `@secret` taint, `@linear` ownership, relational `@verify` solver |
| `engine-rs/src/ir.rs` | Bytecode IR optimization (SSA copy/const propagation, algebraic folding, DCE, escape-analysis `auto_arena`) |
| `engine-rs/src/vm.rs` | Bytecode VM, 2-generation GC + write barriers, LIFO/auto arenas, flight-recorder profiler |
| `engine-rs/src/jit.rs` | In-memory `x86_64` & `aarch64` native JIT compiler |
| `engine-rs/src/spirv.rs` & `gpu.rs` | Vulkan SPIR-V + Direct3D 11 HLSL compute emitters (tiled GEMM, causal FlashAttention, LayerNorm) |
| `engine-rs/src/sim_actor.rs` | OTP supervisors, location-transparent TCP actors, deterministic virtual-clock & crash-safe VFS simulation |
| `engine-rs/src/native_codegen.rs` | Standalone `.wasm` / WASI emitter & direct zero-toolchain PE32+/ELF64 binary writer |
| `compiler-vy/src/main.vy` | 3-stage self-hosted pure-Veyra `.vybc` compiler |
| `tooling/vscode/` | VS Code grammar, LSP client, and time-travel DAP integration |

## High-Impact Areas to Help With

- **Cross-platform verification:** Testing direct ELF64 `veyra aot --direct --elf` and macOS `aarch64` JIT paths on Linux/macOS hardware.
- **Standard library & examples:** Building real-world apps and benchmarks in `examples/` using `vyhttp`, `vysqlite`, `vyframe`, and `vytensor`.
- **Self-hosted compiler passes:** Expanding IR optimizations and diagnostics in `compiler-vy/src/main.vy` while preserving the 3-stage fixpoint (`Stage 2 == Stage 3`).
- **Editor tooling:** Improving syntax scopes, snippets, and DAP UX in `tooling/vscode/`.
