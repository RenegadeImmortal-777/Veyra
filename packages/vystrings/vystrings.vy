fn starts(text: String, prefix: String) -> Bool { return starts_with(text, prefix); }
fn ends(text: String, suffix: String) -> Bool { return ends_with(text, suffix); }
fn replace(text: String, from: String, to: String) -> String { return replace_text(text, from, to); }
fn words(text: String) -> List { return split(trim(text), " "); }
