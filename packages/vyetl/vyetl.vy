fn load_csv(path: String) -> List { return csv_read(path); }
fn save_csv(path: String, rows: List) -> Null { return csv_write(path,rows); }
fn select_columns(rows: List, names: List) -> List { return table_select(rows,names); }
fn sort_rows(rows: List, name: String) -> List { return table_sort(rows,name); }
fn clean_nulls(rows: List, name: String) -> List { return table_drop_nulls(rows,name); }
fn fill_missing(rows: List, name: String, value: Any) -> List { return table_fill_null(rows,name,value); }
fn group_counts(rows: List, name: String) -> List { return table_group_count(rows,name); }
fn join_inner(left: List, right: List, left_key: String, right_key: String) -> List {
    return table_inner_join(left,right,left_key,right_key);
}
fn join_left(left: List, right: List, left_key: String, right_key: String) -> List {
    return table_left_join(left,right,left_key,right_key);
}
