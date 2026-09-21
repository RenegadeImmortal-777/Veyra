# Veyra 11.0 Nexus

Nexus is the ecosystem-focused successor to Veyra 10 Zenith.

## What changed

### Ecosystem and packaging
- Registry expanded from 52 to 60 native Veyra packages.
- Added vyarray, vyvalidate, vyauth, vyquery, vyserde, vyetl, vymodel and vyapi.
- Added hosted registry support through VEYRA_REGISTRY_URL.
- Added HTTP package downloads for single-file and multi-file packages.
- Added recursive package dependencies with cycle protection.
- Added path-traversal protection for downloaded package files.
- Existing local/offline registry behavior remains the default.
- Lockfile SHA-256 integrity auditing remains mandatory.

### Native ML/tensor stack
- Added tensor division with reverse-mode gradients.
- Added tensor negation and scalar-power autograd.
- Added numerically stable row-wise softmax for 1D/2D tensors with gradients.
- Added detach and numel helpers.
- Added persistent Adam optimizer state, bias correction and reset support.
- Existing SGD, serialization, matmul, reshape/transpose and activation gradients remain supported.

### Web/application runtime
- Dynamic app handlers now receive request headers.
- Handlers may return arbitrary response headers.
- This enables cookies, CORS, cache controls, authorization headers and request IDs at application level.
- vyweb and vyapi expose header-oriented helpers.

### Developer tooling
- Native LSP completion surface expanded for the new tensor and task APIs.
- VS Code builtin completion list updated with Nexus primitives.
- Ecosystem integration tests are now repeatable release gates.

## New release gates

In addition to formatting, unit tests, portable-core compilation, warnings-as-errors clippy, optimized builds and the package matrix, Nexus adds an ecosystem gate that verifies:

1. a hosted registry can be served and fetched over HTTP;
2. a remote package dependency is recursively installed;
3. the downloaded packages compile and execute;
4. the resulting lockfile passes integrity audit;
5. HTTP request headers reach a Veyra app handler;
6. custom response headers are returned to a real HTTP client.

## Release result

Nexus passed the full release gate on September 21, 2026:

- cargo fmt --check — PASS
- cargo test — PASS, 57/57 tests
- cargo check --no-default-features — PASS
- cargo clippy --all-targets --all-features -- -D warnings — PASS
- cargo build --release — PASS
- registry package matrix — PASS, 60/60 packages
- hosted registry + recursive dependency gate — PASS
- remote package run + SHA-256 audit — PASS
- real HTTP request/response header gate — PASS
- installed CLI — PASS, Veyra 11.0.0-nexus
- installed VS Code extension — PASS, veyra.veyra-language@11.0.0
- installed package tamper detection and restore — PASS

## Measurement

Nexus materially improves the package-management, application-framework and ML portions of Veyra. It still does not claim equivalence with the scale of PyPI, NumPy/SciPy, PyTorch/JAX, Django/FastAPI, decades of platform compatibility, or a global third-party community.

The V11 goal is to make those ecosystem gaps increasingly addressable by external packages rather than requiring every capability to be hard-coded into the Veyra engine.
