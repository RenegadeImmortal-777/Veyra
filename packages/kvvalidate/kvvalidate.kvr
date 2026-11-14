fn required(value: Any) -> Bool {
    if value==null { return false; }
    if type_of(value)=="String" { return len(value)>0; }
    return true;
}
fn min_length(value: String, minimum: Int) -> Bool { return len(value)>=minimum; }
fn max_length(value: String, maximum: Int) -> Bool { return len(value)<=maximum; }
fn length_between(value: String, minimum: Int, maximum: Int) -> Bool {
    return len(value)>=minimum and len(value)<=maximum;
}
fn number_between(value: Number, minimum: Number, maximum: Number) -> Bool {
    return value>=minimum and value<=maximum;
}
fn email(value: String) -> Bool {
    return regex_match("^[^@ ]+@[^@ ]+\\.[^@ ]+$",value);
}
fn identifier(value: String) -> Bool {
    return regex_match("^[A-Za-z_][A-Za-z0-9_]*$",value);
}
fn one_of(value: Any, choices: List) -> Bool { return contains(choices,value); }
fn all_required(values: List) -> Bool {
    for value in values { if not required(value) { return false; } }
    return true;
}
