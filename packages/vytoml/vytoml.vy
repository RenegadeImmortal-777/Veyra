fn parse(text: String) -> Any { return toml_parse(text); }
fn stringify(value: Any) -> String { return toml_stringify(value); }
