> **Historical record.** Superseded by [STATUS.md](STATUS.md) and
> [CHANGELOG.md](CHANGELOG.md). Retained for provenance.

# Veyra 8.0 Elevation

Veyra 8.0 Elevation is the first release aimed at a defensible ~50%-overall comparison against Python-in-its-entirety, while keeping ecosystem/community maturity explicitly separate from compiler/runtime capability.

## Major advances beyond Meridian

- async fn and async methods
- await + gather for task aggregation
- with/as context management
- decorator syntax for callable transformation
- try/catch/finally cleanup semantics
- parameterized web routes and query parameters
- request/response helpers in vyweb
- richer dataframe operations: head/tail, drop/fill nulls, left joins
- ML workflow metrics: accuracy, precision, recall, F1, confusion counts, train/test split
- expanded path/text services
- 47 native packages
- package-matrix automation on Windows and POSIX
- generated package index documentation

## Verification

- 45/45 native regression tests pass
- cargo fmt --check passes
- cargo check --no-default-features passes
- cargo clippy --all-targets --all-features -- -D warnings passes
- 47/47 registry packages independently install/import/check
- parameterized web routing tested live
- package integrity hashing retained
- Python-independent native execution retained

## What "~50% overall" means

The comparison includes the Veyra-owned language/runtime, standard platform, tooling, web/data/science/ML foundations, portability engineering, package infrastructure, documentation/examples, and interoperability.

It does not pretend that 47 packages equal PyPI's scale, or that a new language can instantly reproduce Python's decades of users, tutorials, production deployments, research adoption, commercial support, and third-party maintenance.

Elevation narrows the technical and framework gaps enough that, under the same weighted whole-platform rubric previously used, Veyra enters the ~48-52% overall range.

## Remaining largest gaps

- escaping closure objects returned from nested lambdas/decorators
- full event-loop async I/O and structured cancellation
- multiple inheritance and richer object protocol
- advanced generics/type inference
- optimizing JIT/AOT backend
- full debugger protocol and workspace-wide refactoring
- NumPy/SciPy-class numerical depth
- pandas/Polars-class dataframe breadth
- PyTorch/JAX-class autograd/GPU/tensor stack
- production web middleware/auth/session/ORM/WebSocket depth
- certified Linux/macOS/ARM binaries
- hosted signed public registry
- large independent community/ecosystem
