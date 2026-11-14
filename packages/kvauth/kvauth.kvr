fn password_record(password: String, iterations: Number) -> Map {
    let salt=uuid_v4();
    let digest=pbkdf2_sha256(password,salt,iterations,32);
    return {"salt":salt,"iterations":iterations,"digest":digest};
}
fn verify_password(password: String, record: Map) -> Bool {
    let candidate=pbkdf2_sha256(password,record["salt"],record["iterations"],32);
    return constant_time_equal(candidate,record["digest"]);
}
fn api_key() -> String {
    return replace_text(uuid_v4(),"-","") + replace_text(uuid_v4(),"-","");
}
fn sign(secret: String, message: String) -> String { return hmac_sha256(secret,message); }
fn verify_signature(secret: String, message: String, signature: String) -> Bool {
    return constant_time_equal(hmac_sha256(secret,message),signature);
}
