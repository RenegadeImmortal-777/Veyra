fn check_cases(property: Function, cases: List, message: String) -> Bool {
    for value in cases {
        assert(property(value), message + " case=" + str(value));
    }
    return true;
}
fn check_range(property: Function, start: Int, end: Int, message: String) -> Bool {
    for value in range(start, end) {
        assert(property(value), message + " case=" + str(value));
    }
    return true;
}
