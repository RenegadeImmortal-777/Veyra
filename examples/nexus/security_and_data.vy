# Veyra 11.0 Nexus Showcase — Cryptographic Primitives & Bytecode Compilation

let key = "nexus-secret-key";
let payload = "transfer:1000:VEYRA";
let signature = hmac_sha256(key, payload);
assert(len(signature) == 64);
assert(constant_time_equal(signature, hmac_sha256(key, payload)));

let derived_key = pbkdf2_sha256("correct-horse-battery-staple", "veyra-salt", 200, 32);
assert(len(derived_key) == 64);

let artifact = vybc_compile("let x = 6 * 7; assert(x == 42);");
vybc_run(artifact);
print("Nexus security & .vybc execution verified.");
