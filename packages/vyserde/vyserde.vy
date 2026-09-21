fn json_encode(value: Any) -> String { return json_stringify(value); }
fn json_decode(text: String) -> Any { return json_parse(text); }
fn toml_encode(value: Any) -> String { return toml_stringify(value); }
fn toml_decode(text: String) -> Any { return toml_parse(text); }
fn base64_text(text: String) -> String { return base64_encode(text); }
fn unbase64_text(text: String) -> String { return base64_decode(text); }
