// Sovereign Demo 01: Static @verify Contracts, Taint Flow & Linear Ownership
// Run:
//   .\bin\veyra-native.exe verify examples\sovereign\01_verify_safety.vy
//   .\bin\veyra-native.exe run examples\sovereign\01_verify_safety.vy

@secret
fn load_api_token() -> String {
    return "sk-live-98765";
}

@linear
fn open_handle(name: String) -> String {
    return "handle:" + name;
}

@verify
fn safe_loop_budget(limit: Int) -> Int {
    requires(limit >= 1, "limit >= 1");
    requires(limit <= 100, "limit <= 100");
    let step = 0;
    while step < limit {
        step += 1;
    }
    ensures(step >= 1, "step >= 1");
    return 1000 / step;
}

// Non-escaping class instances inside a helper function are automatically
// reclaimed at function exit by Veyra's escape-analysis auto_arena pass.
class ScratchFrame {
    fn __init__(self, seq: Int) {
        self.seq = seq;
    }
}

fn work_with_auto_arena(n: Int) -> Int {
    let tmp = ScratchFrame(n);
    return tmp.seq * 2;
}

let clean_tok = sanitize(load_api_token());
let h = open_handle("wal-stream");
linear_consume(h);

profile_start();
let ratio = safe_loop_budget(10);
let doubled = work_with_auto_arena(ratio);
let prof = profile_stop();

print("sanitized token:", clean_tok);
print("safe_loop_budget(10) =", ratio, "| auto_arena doubled =", doubled);
print("auto_arena scopes triggered:", prof["gc"]["auto_arena_scopes"]);
