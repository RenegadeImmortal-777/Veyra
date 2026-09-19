# Veyra Language Specification — 2026 Convergence Edition

Veyra source files use UTF-8 and the .vy extension.

## Core language
Declarations: let, const, fn, class. Control flow: if/else, while, for/in, return, break, continue. Error flow: try/catch and throw. Values include Null, Bool, Int, Float, String, List, Tuple, Set, Map, Function, Class and user instances.

Functions support typed parameters, return annotations, closures through lexical environments, recursion and bound instance methods. Classes use methods declared inside a class body; init(self, ...) is the constructor. Member access uses object.member and member assignment is supported.

## Interoperability
py_import(name) imports an installed Python module. Python objects participate in Veyra member access and calls. py_call(fn, args, kwargs) supports Python keyword arguments. py_get, py_set and py_dir provide reflection helpers. Project-local Python packages are isolated in .veyra/venv and managed by the Veyra CLI.

## Runtime services
Built-ins include JSON serialization/parsing, regular expressions, files, paths, environment access, HTTP GET, SQLite connections, shell subprocesses, random numbers and thread-pool parallel_map.

## Projects
veyra.json is the manifest. veyra add/remove/packages manage dependencies, veyra lock captures exact installed Python dependency versions, veyra install restores them and veyra audit validates the dependency graph.

## Native engine
The Rust Horizon engine remains a separate bytecode VM path. The compatibility/Convergence runtime currently has broader language semantics; native coverage is expanded without breaking .vy source compatibility.
