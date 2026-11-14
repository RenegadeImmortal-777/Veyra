fn available() -> Bool { return jit_available(); }
fn set_enabled(flag: Bool) -> Bool { return jit_set_enabled(flag); }
fn compile(target: Any) -> Bool { return jit_compile(target); }
fn info() -> Map { return jit_info(); }
fn stats() -> Map { return jit_stats(); }
fn backend() -> String {
    let m = jit_info();
    return m["backend"];
}
fn bench(fn_target: Any, arg: Any, iterations: Number) -> Map {
    jit_compile(fn_target);
    let t0 = clock();
    let last = null;
    let i = 0;
    while i < iterations {
        last = fn_target(arg);
        i = i + 1;
    }
    let elapsed_ms = (clock() - t0) * 1000.0;
    return {
        "iterations": iterations,
        "elapsed_ms": elapsed_ms,
        "last": last,
        "jit": jit_stats()
    };
}
