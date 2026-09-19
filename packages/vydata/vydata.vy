fn read_csv(path: String) -> List { return csv_read(path); }
fn write_csv(path: String, rows: List) -> Void { csv_write(path, rows); }
fn cell(rows: List, row: Int, name: String) -> Any { return rows[row][name]; }
fn row_count(rows: List) -> Int { return len(rows); }
fn column(rows: List, name: String) -> List { return table_column(rows, name); }
fn where_equal(rows: List, name: String, value: Any) -> List { return table_filter_eq(rows, name, value); }
fn describe(rows: List, name: String) -> Map { return table_describe(rows, name); }
