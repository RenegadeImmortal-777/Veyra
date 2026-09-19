fn available(library: String) -> Bool { return ffi_available(library); }
fn call_i32(library: String, symbol: String, args: List) -> Number { return ffi_i32(library, symbol, args); }
fn call_i64(library: String, symbol: String, args: List) -> Number { return ffi_i64(library, symbol, args); }
fn call_f64(library: String, symbol: String, args: List) -> Number { return ffi_f64(library, symbol, args); }
