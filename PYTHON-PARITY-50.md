# Veyra vs Python — Meridian 50% Technical Milestone

## What "50%" means

The score is a technical engineering comparison, not an ecosystem-size claim.

Included in the technical score:
1. language semantics
2. runtime/app execution
3. standard services
4. project and package tooling
5. IDE/LSP/debugging
6. web foundations
7. data/science foundations
8. ML foundations
9. portability engineering
10. package infrastructure

Not included as if they could be manufactured in one build session:
- number of users
- number of third-party packages
- decades of deployment history
- books/courses/community answers
- scientific-paper adoption
- commercial support network

Those remain independently reported and are still tiny relative to Python.

## Evidence gates

A Meridian release is not considered valid unless all of these pass:

- cargo fmt --check
- cargo test
- cargo check --no-default-features
- cargo clippy --all-targets --all-features -- -D warnings
- all local-registry packages install/import/check independently
- representative language/stdlib/data/science smoke test
- optimized release build
- installed binary smoke test
- packaged app runs with Python absent from PATH
- package audit detects deliberate tampering
- VS Code extension version matches compiler release

## Why Meridian crosses the technical 50%-class threshold

Apex already supplied typed OOP, inheritance, exceptions, lexical captures, concurrency, await, FFI, web handlers, data/ML foundations and hardened packages.

Meridian closes several large language/tooling gaps:
- yield/generators
- generator iteration
- comprehensions
- match/case
- lambdas and named first-class functions
- higher-order functional APIs
- richer numerical linear algebra
- broader dataframe aggregation
- broader plotting
- config/datetime/encoding/UUID services
- generic HTTP requests
- platform introspection
- LSP definition/references
- cross-platform CI and portable-core build checks

That moves the technical platform from the prior ~25–30% whole-platform comparison into a defensible ~50%-class technical capability range, while ecosystem/community parity remains separately much lower.
