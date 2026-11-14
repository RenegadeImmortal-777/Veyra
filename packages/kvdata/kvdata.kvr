fn read_csv(path: String) -> List { return csv_read(path); }
fn write_csv(path: String, rows: List) -> Void { csv_write(path, rows); }
fn cell(rows: List, row: Int, name: String) -> Any { return rows[row][name]; }
fn row_count(rows: List) -> Int { return len(rows); }
fn column(rows: List, name: String) -> List { return table_column(rows, name); }
fn where_equal(rows: List, name: String, value: Any) -> List { return table_filter_eq(rows, name, value); }
fn describe(rows: List, name: String) -> Map { return table_describe(rows, name); }
fn head(rows: List, count: Int) -> List { return table_head(rows, count); }
fn tail(rows: List, count: Int) -> List { return table_tail(rows, count); }
fn drop_nulls(rows: List, name: String) -> List { return table_drop_nulls(rows, name); }
fn fill_null(rows: List, name: String, value: Any) -> List { return table_fill_null(rows, name, value); }
fn group_count(rows: List, name: String) -> List { return table_group_count(rows, name); }
fn group_sum(rows: List, group: String, value: String) -> List { return table_group_sum(rows, group, value); }
fn group_mean(rows: List, group: String, value: String) -> List { return table_group_mean(rows, group, value); }
fn sort_by(rows: List, name: String) -> List { return table_sort(rows, name); }
fn select(rows: List, names: List) -> List { return table_select(rows, names); }
fn inner_join(left: List, right: List, left_key: String, right_key: String) -> List {
    return table_inner_join(left, right, left_key, right_key);
}
fn left_join(left: List, right: List, left_key: String, right_key: String) -> List {
    return table_left_join(left, right, left_key, right_key);
}
