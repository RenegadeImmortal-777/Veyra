# Veyra 6.0 Apex

Veyra is a native-first programming language and application platform built around its own Rust lexer, parser, semantic pass, bytecode compiler and VM.

The default `veyra` execution path does not require Python.

## Language

Veyra now supports functions and recursion, constants and typed variables, lists/maps/indexing, modules, native classes and inheritance, constructors and mutable object fields, lexical captures, try/catch/throw, general iteration, break/continue and native `await` over background tasks.

Example:

    class Patient {
        fn init(self, name: String, age: Int) -> Null {
            self.name = name;
            self.age = age;
            return null;
        }

        fn label(self) -> String {
            return self.name;
        }
    }

    let patient: Patient = Patient("Ama", 24);

Types are enforced for annotated declarations, assignments, function arguments, return values and class inheritance relationships.

## Concurrency

    fn square(n: Int) -> Int {
        return n * n;
    }

    let task = spawn("square", [9]);
    let result = await task;

Message-passing channels are available through `channel`, `send`, `recv` and `try_recv`.

## Native interop

Veyra can call native dynamic libraries directly with constrained C-ABI entrypoints:

    ffi_i32(library, symbol, args)
    ffi_i64(library, symbol, args)
    ffi_f64(library, symbol, args)

## Dynamic web applications

    veyra app app.vy routes.json 8080

Routes can invoke functions implemented directly in Veyra and return text, JSON or structured response maps. Veyra objects can persist state across requests.

## Data / science / ML

The native runtime includes vector/matrix operations, descriptive statistics, CSV, dataframe-style table transforms and joins, SVG plotting, linear/logistic regression, k-NN, k-means and neural-inference primitives.

Packages include `vyscience`, `vydata`, `vyplot` and `vyml`.

## Tooling

    veyra new app
    veyra add vydata
    veyra check
    veyra lint
    veyra run
    veyra test
    veyra bench
    veyra profile
    veyra doc
    veyra audit
    veyra build

Veyra lockfiles include package-content SHA-256 hashes and `veyra audit` verifies vendored package integrity.

See `STATUS-6-APEX.md` for the exact release scope.
