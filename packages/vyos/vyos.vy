fn read(path: String) -> String { return read_file(path); }
fn write(path: String, data: Any) -> Null { return write_file(path, data); }
fn exists(path: String) -> Bool { return path_exists(path); }
fn current_directory() -> String { return cwd(); }
fn environment(name: String) -> Any { return env_get(name); }
fn command(text: String) -> Map { return shell(text); }
fn processors() -> Number { return cpu_count(); }
fn os() -> String { return platform_os(); }
fn architecture() -> String { return platform_arch(); }
fn family() -> String { return platform_family(); }
fn separator() -> String { return path_separator(); }
