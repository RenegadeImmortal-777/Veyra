# Veyra 7.0 Meridian

Veyra is a native-first programming language and application platform built around its own Rust lexer, parser, semantic checker, bytecode compiler and virtual machine.

The default Veyra execution path does not require Python.

## Meridian language

Veyra supports:

- typed variables, function parameters and return values
- functions, recursion, closures and first-class function values
- lambdas
- classes, constructors and inheritance
- mutable object fields and methods
- lists/maps/indexing
- list comprehensions
- generators and yield
- for-in over generators/lists/strings/maps
- if/else, while, break and continue
- match/case
- try/catch/throw
- tasks, await and channels
- native C ABI FFI

Example:

    fn numbers(n: Int) -> Generator {
        let i = 0;
        while i < n {
            yield i;
            i = i + 1;
        }
    }

    let doubled = [x * 2 for x in numbers(5) if x > 1];

    match len(doubled) {
        case 3 { print("three") }
        case _ { print("other") }
    }

## Standard platform

Native facilities include JSON, TOML, regex, Base64, UUIDs, datetime, filesystem/process/environment access, HTTP, SQLite, hashing, platform introspection, GUI dialogs, vector/matrix math, advanced linear algebra, statistics, tables/dataframes, plotting and introductory ML.

## Toolchain

    veyra new app
    veyra add vydata
    veyra check
    veyra lint
    veyra run
    veyra test
    veyra debug
    veyra ast
    veyra symbols
    veyra lsp
    veyra bench
    veyra profile
    veyra doc
    veyra audit
    veyra build

## Package ecosystem

The local registry contains 30 Veyra-native packages. Every package is validated through an independent fresh-project install/import/check matrix.

## Portability

The portable core compiles with native services disabled. CI is defined for Windows, Ubuntu and macOS. This is build readiness, not a claim of full release certification on every OS/architecture.

See STATUS-7-MERIDIAN.md and PYTHON-PARITY-50.md for the exact scope and comparison methodology.
