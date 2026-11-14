# Kovrek Language Specification — 2026 Edition

Kovrek source files use UTF-8 and the `.kvr` extension.

Execution defaults to the native Rust Horizon engine, which does not require Python.
Python interoperability described below is an optional bridge provided by the legacy
Convergence runtime; see [../STATUS.md](../STATUS.md) for the current release state.

## Core language

Declarations: `let`, `const`, `fn`, `class`. Control flow: `if`/`else`, `while`,
`for`/`in`, `return`, `break`, `continue`. Error flow: `try`/`catch`/`finally` and
`throw`. Pattern matching uses `match`/`case`. Resource handling uses `with`/`as`.
Values include Null, Bool, Int, Float, String, List, Tuple, Set, Map, Function, Class
and user instances.

Functions support typed parameters, return annotations, closures through lexical
environments, recursion and bound instance methods. Function values are first-class
and may escape the scope that created them. Classes use methods declared inside a
class body; `init(self, ...)` is the constructor. Member access uses `object.member`
and member assignment is supported. Generators use `yield`. `async fn` declares an
async function, with `await` and `gather` for tasks.

## Interoperability (Convergence runtime only)

`py_import(name)` imports an installed Python module. Python objects participate in
Kovrek member access and calls. `py_call(fn, args, kwargs)` supports Python keyword
arguments. `py_get`, `py_set` and `py_dir` provide reflection helpers. Project-local
Python packages are isolated in `.kovrek/venv` and managed by the Kovrek CLI. The
native engine has no equivalent of this bridge.

## Runtime services

Built into the native engine: JSON serialization/parsing, TOML, CSV, regular
expressions, files, paths, environment access, subprocesses, HTTP client, SQLite
connections, SHA-256/HMAC/PBKDF2, random numbers, date/time, Base64, UUID, platform
introspection, thread-pool `parallel_map`, native tasks with channels, and a tensor
runtime with reverse-mode autograd.

## Projects

`kovrek.json` is the manifest. `kovrek add`/`remove`/`packages` manage dependencies,
`kovrek lock` captures exact installed versions, `kovrek install` restores them, and
`kovrek audit` validates the dependency graph and package integrity.

## Engines

Two engines ship in this repository. The native Horizon engine is the default and
implements the language surface above without Python. The legacy Convergence runtime
implements the earlier Kovrek 1 surface in Python and uniquely provides the
`py_import` bridge. Both consume the same `.kvr` source. The native engine is the
migration target and, as of Kovrek 11, has the broader language coverage; the
Convergence runtime's remaining distinct value is Python interoperability.
