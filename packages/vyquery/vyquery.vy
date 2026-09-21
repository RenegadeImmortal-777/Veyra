fn select_all(table: String) -> String { return "SELECT * FROM " + table; }
fn select_where(table: String, column: String) -> String {
    return "SELECT * FROM " + table + " WHERE " + column + " = ?";
}
fn delete_where(table: String, column: String) -> String {
    return "DELETE FROM " + table + " WHERE " + column + " = ?";
}
fn count_all(table: String) -> String { return "SELECT COUNT(*) AS count FROM " + table; }
fn insert_one(table: String, columns: List) -> String {
    let names=join(columns,",");
    let marks=[];
    let i=0;
    while i<len(columns) { marks=append(marks,"?"); i=i+1; }
    return "INSERT INTO " + table + " (" + names + ") VALUES (" + join(marks,",") + ")";
}
fn update_one(table: String, columns: List, key: String) -> String {
    let parts=[];
    for column in columns { parts=append(parts,column + " = ?"); }
    return "UPDATE " + table + " SET " + join(parts,", ") + " WHERE " + key + " = ?";
}
