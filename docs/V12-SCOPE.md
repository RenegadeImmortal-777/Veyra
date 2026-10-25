# Veyra 12 Scope

Nexus (11.0) has no successor defined. Two candidate targets have been carried in
every milestone document's "remaining gaps" list since Apex: a **self-hosted
compiler** and an **optimizing/JIT backend**. This document scopes both, recommends
an order, and defines the gate that would prove each one.

## Prerequisite: a verifiable build

Neither target should be started while the build is unverifiable. Both are
multi-session changes to a 9,495-line file, and neither can be validated by reading.
The release gates must be runnable first — see [../STATUS.md](../STATUS.md),
"Verification status". This is a hard sequencing constraint, not a preference.

## Prerequisite: split the engine

`engine-rs/src/main.rs` contains the lexer, parser, checker, bytecode compiler, VM,
LSP, registry, HTTP server, SQLite bindings and tensor runtime in one file.

Both targets need a modular compiler with separable passes:

- an optimizing backend inserts a new stage between bytecode and execution, and
  needs a stable IR plus a pass pipeline;
- a self-hosted compiler needs the existing passes to be callable as a library
  rather than reachable only through the CLI entry point.

Splitting the crate is therefore step one of either path. It is also the lowest-risk
work with the highest confidence of success: pure reorganization, verifiable by the
existing test suite passing unchanged.

## Option A: optimizing/JIT backend

Pipeline: bytecode → typed IR → optimization passes → register-allocated execution
or native codegen.

Incremental by construction. Value arrives early and in stages:

1. Constant folding and dead-code elimination on the existing bytecode.
2. Constant-propagating and strength-reducing passes over a typed IR.
3. Inline caches for the dynamic dispatch paths (`CallMethod`, map indexing).
4. Register allocation replacing the stack machine's push/pop traffic.
5. Optional native codegen for hot functions.

**Why this first.** It is measurable against a baseline that already exists, it
benefits every user rather than a subset, and it is the thing that makes
self-hosting practical — a self-hosted compiler requires no GC and no optimizer,
waiting on determinism. It is also compatible with the current architecture: no new
language semantics are required, and semantics can be held fixed while performance
changes, which makes the gate crisp.

**Risk.** Real but bounded. A typed IR requires committing to a type representation
the VM currently enforces dynamically, so this is where the runtime type enforcement
in Apex/Zenith pays off or costs.

## Option B: self-hosted compiler

Path: stage0 (Rust) compiles a stage1 compiler written in Veyra; stage1 compiles the
compiler again into stage2; stage1 and stage2 must agree.

**Why it is attractive.** It is the strongest possible demonstration that the
language is complete enough to build real software, and it surfaces genuine gaps
fast: writing a compiler exercises strings, maps, file I/O, error handling,
recursion and performance in ways the current test suite does not. Self-hosting is
also the standard credibility milestone for a new language.

**Why it should follow the backend.** With no optimizer and no memory model, a
self-hosted compiler would be slow enough to make the compile-itself loop painful,
and the work would not improve performance for anyone else. The missing pieces are
not only performance: there is no garbage collector or formal memory model, and the
remaining gaps list still carries "rich generics, type inference and protocols" —
all of which a compiler front end is exactly the kind of program that wants.

## Gate definitions

A target is not "done" because code was written. Define the gate before starting.

### Optimizing backend gate

- Semantic equivalence: the full native suite passes identically with optimization
  enabled and disabled. Optimized and unoptimized execution produce the same values
  for every test in `mod tests`, and the same program output byte for byte.
- Differential testing: a fuzz/degenerate-program corpus runs under both paths and
  agrees, including on error messages and thrown exception values.
- Measured speedup: named benchmarks recorded in the repository, with before/after
  numbers and the machine recorded. A claimed speedup without a measurement is not a
  result.
- No package regressions: the 60/60 package matrix still installs, imports and runs.
- Portable-core build (`--no-default-features`) still passes, and the optimizer is
  feature-gated so the core builds without it.

### Self-hosting gate

- Fixpoint: stage1 and stage2 compilers produce byte-identical output for the whole
  test corpus. A single stage1 that merely happens to work is not a fixpoint.
- The self-hosted compiler passes every gate above, including the package matrix.
- The self-hosted compiler builds and runs a project from the registry as an end-to-
  end check, not just the internal test suite.
- Recorded compile-time comparison against stage0, so "slow but correct" is a
  stated property rather than a surprise.

## Recommendation

1. Make the gates runnable.
2. Split `engine-rs` into modules without changing behavior.
3. Build the optimizing backend against a measured baseline.
4. Then attempt self-hosting, with the optimizer and the resulting performance
   picture in hand.

If the goal is credibility rather than capability, A and B swap — self-hosting is the
more impressive claim and the harder one to fake. If the goal is a better language,
the backend comes first, because it is what makes the self-hosted compiler worth
using once it exists.
