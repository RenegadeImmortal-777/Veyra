# Veyra 8.0 Elevation

Veyra is a native-first programming language and application platform built around its own Rust lexer, parser, semantic checker, bytecode compiler and VM.

Elevation adds async functions, gather, context managers, decorators, finally, parameterized web routing, deeper dataframe operations, ML metrics and a 47-package native registry.

Core examples:

    async fn square(n: Int) -> Int {
        return n * n;
    }

    let a = square(3);
    let b = square(4);
    let results = gather([a, b]);

    class Resource {
        fn enter(self) -> Resource { return self; }
        fn exit(self) -> Null { return null; }
    }

    with Resource() as r {
        print("managed");
    }

    try {
        throw "boom";
    } catch err {
        print(err);
    } finally {
        print("cleanup");
    }

Web handlers support route params and query params:

    GET /users/{id}?q=hello

Data helpers include head/tail, filtering, grouping, null handling, inner/left joins, statistics and plotting.

ML helpers include regression/classification foundations plus accuracy, precision, recall, F1 and train/test splitting.

The default Veyra execution path does not require Python.

See STATUS-8-ELEVATION.md, PYTHON-OVERALL-50.md and docs/PACKAGES.md.
