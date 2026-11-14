fn hash(value: Any) -> String { return sha256(value); }
fn hmac(key: String, message: String) -> String { return hmac_sha256(key, message); }
fn derive(password: String, salt: String, iterations: Number, bytes: Number) -> String {
    return pbkdf2_sha256(password, salt, iterations, bytes);
}
fn secure_equal(left: String, right: String) -> Bool { return constant_time_equal(left, right); }
