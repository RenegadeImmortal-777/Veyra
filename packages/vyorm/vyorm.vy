fn execute(path: String, sql: String) -> Number { return sqlite_exec(path, sql); }
fn query(path: String, sql: String) -> List { return sqlite_query(path, sql); }
fn all(path: String, table: String) -> List { return sqlite_query(path, "SELECT * FROM " + table); }
fn where(path: String, table: String, clause: String) -> List { return sqlite_query(path, "SELECT * FROM " + table + " WHERE " + clause); }
fn count(path: String, table: String) -> List { return sqlite_query(path, "SELECT COUNT(*) AS count FROM " + table); }
fn create(path: String, table: String, columns: String) -> Number { return sqlite_exec(path, "CREATE TABLE IF NOT EXISTS " + table + " (" + columns + ")"); }
