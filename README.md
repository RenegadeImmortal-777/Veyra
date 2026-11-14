# Kovrek 12.0 "Sovereign"

> **Python ergonomics. Single-binary Rust runtime (`8.8 MB`). Static `@verify` proofs, GPU FlashAttention/tiled GEMM (`SPIR-V` + `HLSL`), crash-safe deterministic actors, and zero-toolchain native/Wasm AOT — with zero Python dependency.**

[![Release Binaries](https://img.shields.io/badge/release-v12.0.1--kovrek-0ea5e9)](https://github.com/RenegadeImmortal-777/Kovrek/releases/tag/v12.0.1-kovrek)
[![Tests](https://img.shields.io/badge/cargo%20test-130%2F130%20pass-10b981)](STATUS.md)
[![Packages](https://img.shields.io/badge/packages-74%2F74%20verified-10b981)](docs/PACKAGES.md)
[![Gates](https://img.shields.io/badge/release%20gates-8%2F8%20pass-10b981)](STATUS.md)

---

## Why Kovrek Exists

Shipping real AI + systems software usually forces teams to glue together **5–7 separate toolchains**: Python for iteration, Rust/C++ for binaries, CUDA/HLSL for GPU compute, TLA+/Dafny/manual tests for invariants, Erlang/FoundationDB-style harnesses for fault injection, and another toolchain for WebAssembly.

**Kovrek collapses those boundaries into one self-hosted native language platform:**
- Write clean, high-level syntax (full OOP with C3 MRO/`@property`/metaclasses, `match`/`case`, async/await, tensors).
- Prove contracts, taint sanitization, linear resource moves, and loop invariants statically (`kovrek verify`).
- Simulate hard power crashes & partitions deterministically (`sim_fs_crash`, `sim_partition`, `actor_node_listen`).
- Execute on bytecode VM + superinstructions, in-memory `x86_64`/`aarch64` JIT, portable `SPIR-V`/`HLSL` GPU shaders, standalone `.wasm`, or direct zero-linker PE32+/ELF64 binaries (`kovrek aot --direct`).

---

## 15-Second Tour (`@verify` + Crash Simulation + TCP Actors)

Save as `tour.kvr` and run with `kovrek verify tour.kvr` and `kovrek run tour.kvr`:

```kovrek
@secret
fn load_token() -> String { return "sk-live-secret"; }

@verify
fn safe_budget(limit: Int) -> Int {
    requires(limit >= 1, "limit >= 1");
    let step = 0;
    while step < limit { step += 1; }
    ensures(step >= 1, "step >= 1");
    return 1000 / step; // proven non-zero at compile time
}

// Deterministic fault-injectable crash simulation: keep fsync'd WAL, lose dirty writes
sim_start(42);
sim_fs_write("/wal/ckpt.json", "{\"epoch\":1}");
sim_fs_sync();
sim_fs_write("/wal/dirty.tmp", "uncommitted");
let crash = sim_fs_crash();
print("Clean tok:", sanitize(load_token()), "| Ratio:", safe_budget(10));
print("Recovered WAL:", sim_fs_read("/wal/ckpt.json"), "| Lost dirty:", crash["lost_dirty"]);
sim_stop();
```

---

## Quick Start

### Option A: Prebuilt Native Binary (Recommended)
1. Download the `v12.0.1-kovrek` binary for Windows/Linux/macOS from [GitHub Releases](https://github.com/RenegadeImmortal-777/Kovrek/releases/tag/v12.0.1-kovrek) (or run `.\INSTALL.bat` on Windows from a clone).
2. Try the built-in demos right away:

```powershell
# Static formal verifier + taint + linear ownership + auto_arena
.\bin\kovrek-native.exe verify examples\sovereign\01_verify_safety.kvr
.\bin\kovrek-native.exe run examples\sovereign\01_verify_safety.kvr

# Deterministic crash-safe VFS + location-transparent TCP actors
.\bin\kovrek-native.exe run examples\sovereign\02_crash_sim_actors.kvr

# GPU tiled GEMM / causal FlashAttention / bf16 / Autograd + profiler
.\bin\kovrek-native.exe profile examples\sovereign\03_gpu_flash_autograd.kvr

# Direct native binary or standalone Wasm emission (zero C toolchain required)
.\bin\kovrek-native.exe aot --direct examples\hello.kvr hello.exe
.\bin\kovrek-native.exe wasm examples\hello.kvr -o hello.wasm
```

### Option B: Build from Source (`cargo`)
```bash
git clone https://github.com/RenegadeImmortal-777/Kovrek.git
cd Kovrek
cargo build --manifest-path engine-rs/Cargo.toml --release
cargo test --manifest-path engine-rs/Cargo.toml
```

---

## Verified Showcase Examples

Every file in [examples/sovereign/](examples/sovereign) is verified directly against [bin/kovrek-native.exe](bin/kovrek-native.exe):

| Demo | Features Exercised | Command |
|---|---|---|
| [01_verify_safety.kvr](examples/sovereign/01_verify_safety.kvr) | `@verify` loop invariants (`3` proven contracts), `@secret`/`sanitize(...)`, `@linear` resource consume, `auto_arena` escape reclamation | `kovrek verify` / `kovrek run` |
| [02_crash_sim_actors.kvr](examples/sovereign/02_crash_sim_actors.kvr) | Fault-injectable VFS (`sim_fs_sync`/`sim_fs_crash`), supervised OTP actors + TCP RPC (`actor_node_listen`/`actor_remote_ask`) | `kovrek run` |
| [03_gpu_flash_autograd.kvr](examples/sovereign/03_gpu_flash_autograd.kvr) | `gpu_kernel_inspect` (`tiled_matmul` + `attention_causal` SPIR-V/HLSL), `gpu_fused_layernorm`, `bf16` cast, reverse-mode autograd + Adam, `profile_start`/`profile_stop` | `kovrek run` / `kovrek profile` |

---

## Platform Capabilities at a Glance

| Pillar | Capabilities Implemented & Tested |
|---|---|
| **Language & Parity** | C3 MRO multiple inheritance, `super()`, `@property`, metaclasses, stacked decorators, closures/upvalues, generators, comprehensions, full structural `match`/`case`, async/await + `gather` + channels, `BigInt`/`BigDecimal`/`Complex`/`FFT` |
| **Formal Safety & Memory** | `@verify` relational difference-bound solver (joins, loop invariants, callee `ensures`, counterexamples), `@secret`/`sanitize(...)` taint flow, `@linear` ownership across branches/actors/remotes, transitive `@pure`/`@effect(...)`, capability sandbox (`--caps`), 2-gen nursery/old-gen GC + write barriers + LIFO `arena_scope(fn)` + compile-time `auto_arena` |
| **Compilation Targets** | 3-stage self-hosted `.kvbc` compiler (`compiler-kvr/src/main.kvr`), SSA/algebraic/DCE optimizer (`ir.rs`), in-memory `x86_64` & `aarch64` JIT (`jit.rs`), standalone `.wasm`/WASI emitter (`kovrek wasm`), direct zero-toolchain PE32+/ELF64 writer (`kovrek aot --direct`), full `rustc` native AOT |
| **GPU, AI & Python Interop** | Vulkan SPIR-V + Direct3D 11 (`cs_5_0` HLSL) compute + multi-threaded CPU fallback, single-pass `gpu_fuse` & `gpu_fused_linear`, workgroup `groupshared` tiled GEMM, causal online FlashAttention, fused Residual+LayerNorm+activation, `f16`/`bf16` casting, N-D autograd tensors + Adam, in-process CPython C-API + PEP 3118 + strided PyCapsule DLPack (`tensor_to_dlpack`/`py_from_dlpack`) |
| **Fault Simulation & Tooling** | OTP supervision trees (`one_for_one`/`one_for_all`/`rest_for_one`), TCP actor RPC, deterministic SplitMix64 cluster & crash-safe VFS simulation, LSP + time-travel reversible DAP (`stepBack`, forward/reverse data watchpoints), reactive topological DAG notebooks (`kovrek notebook`), flight-recorder profiler (`profile_start`/`profile_stop`/`kovrek profile`), VS Code extension |
| **Ecosystem & FFI** | 74 verified packages (`package-matrix.ps1`), HMAC-SHA256 signed bundles (`.kvpkg`), semver + workspaces, HTTP registry (`kovrek pkg serve`), `kovrek_plugin_v1` native `cdylib` ABI, C header struct/enum/`#define` bindgen (`kovrek ffi bindgen`), dynamic `x86_64` C-ABI `ffi_call` + bounded `mem_*` buffers |

---

## Verification & Honest Scope

All numbers below come directly from [STATUS.md](STATUS.md) and [gate-report.txt](gate-report.txt) generated on this repo:
- **`cargo test`:** `130 passed; 0 failed` (VM, JIT, GPU, Formal Safety, Self-Hosted 3-stage fixpoint, Actors, Sim VFS, DLPack, CPython, AOT, Wasm)
- **Package Matrix:** `74/74` packages checked and executed cleanly
- **Release Gates:** `8/8` (`cargo fmt --check`, `cargo test`, `--no-default-features`, `clippy -D warnings`, `release` build, `package-matrix`, `ecosystem-gate`, `release-check`)

**Where mature ecosystems still lead:** Python (PyPI), Rust (`crates.io`), and TypeScript (`npm`) carry orders-of-magnitude larger third-party library ecosystems, decades of LLVM micro-architectural tuning, and vendor-tuned cuBLAS/Triton kernels. Kovrek is designed to eliminate multi-language glue friction while staying C-ABI and DLPack interoperable when you want external libraries.

---

## Documentation & Contributing

- [STATUS.md](STATUS.md) — Authoritative test/gate report and architecture map
- [docs/LANGUAGE.md](docs/LANGUAGE.md) — Language syntax, contracts, actors, simulation & profiling guide
- [docs/index.html](docs/index.html) — Interactive web showcase ([examples/sovereign/](examples/sovereign) snippets)
- [docs/PACKAGES.md](docs/PACKAGES.md) & [docs/REMOTE-REGISTRY.md](docs/REMOTE-REGISTRY.md) — Package ecosystem & registry protocol
- [CHANGELOG.md](CHANGELOG.md) — Complete release history (`4.0` -> `12.0 Sovereign`)
- [CONTRIBUTING.md](CONTRIBUTING.md) — How to test, build, and contribute
