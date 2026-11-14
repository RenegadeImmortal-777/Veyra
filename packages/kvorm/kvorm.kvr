fn execute(path: String, sql: String) -> Number { return sqlite_exec(path, sql); }
fn execute_params(path: String, sql: String, params: List) -> Number { return sqlite_exec_params(path, sql, params); }
fn query(path: String, sql: String) -> List { return sqlite_query(path, sql); }
fn query_params(path: String, sql: String, params: List) -> List { return sqlite_query_params(path, sql, params); }
fn all(path: String, table: String) -> List { return sqlite_query(path, "SELECT * FROM " + table); }
fn where(path: String, table: String, clause: String) -> List { return sqlite_query(path, "SELECT * FROM " + table + " WHERE " + clause); }
fn count(path: String, table: String) -> List { return sqlite_query(path, "SELECT COUNT(*) AS count FROM " + table); }
fn create(path: String, table: String, columns: String) -> Number { return sqlite_exec(path, "CREATE TABLE IF NOT EXISTS " + table + " (" + columns + ")"); }
fn insert(path: String, table: String, columns: String, placeholders: String, params: List) -> Number {
    return sqlite_exec_params(path, "INSERT INTO " + table + " (" + columns + ") VALUES (" + placeholders + ")", params);
}
