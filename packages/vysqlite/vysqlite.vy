fn execute(path: String, sql: String) -> Number { return sqlite_exec(path, sql); }
fn query(path: String, sql: String) -> List { return sqlite_query(path, sql); }
