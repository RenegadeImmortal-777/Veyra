fn abi_version() -> Number { return 1; }
fn load(path: String) -> Map { return plugin_load(path); }
fn info(handle: Any) -> Map { return plugin_info(handle); }
fn call(handle: Any, fn_name: String, args: List) -> Any {
    return plugin_call(handle, fn_name, args);
}
fn call0(handle: Any, fn_name: String) -> Any {
    return plugin_call(handle, fn_name, []);
}
fn list() -> List { return plugin_list(); }
fn build(src_rs: String, out_path: String) -> String {
    return plugin_build(src_rs, out_path);
}
