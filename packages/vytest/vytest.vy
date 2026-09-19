fn expect(condition: Bool, message: String) -> Bool {
    if not condition {
        throw "test failed: " + message;
    }
    print("PASS", message);
    return true;
}

fn expect_equal(a: Any, b: Any, message: String) -> Bool {
    return expect(a == b, message);
}

fn expect_close(a: Number, b: Number, tolerance: Number, message: String) -> Bool {
    return expect(abs(a - b) <= tolerance, message);
}
