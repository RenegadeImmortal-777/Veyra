fn expect(condition: Bool, message: String) -> Bool {
    if not condition {
        print("FAIL", message);
        return false;
    }
    print("PASS", message);
    return true;
}
fn expect_equal(a: Any, b: Any, message: String) -> Bool {
    return expect(a == b, message);
}
