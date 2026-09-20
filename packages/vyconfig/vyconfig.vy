fn parse_toml(text: String) -> Any { return toml_parse(text); }
fn stringify_toml(value: Any) -> String { return toml_stringify(value); }
fn parse_json(text: String) -> Any { return json_parse(text); }
fn stringify_json(value: Any) -> String { return json_stringify(value); }
