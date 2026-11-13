fn escape_html(text: String) -> String {
    let out = replace(text, "&", "&amp;");
    out = replace(out, "<", "&lt;");
    out = replace(out, ">", "&gt;");
    out = replace(out, "\"", "&quot;");
    out = replace(out, "'", "&#39;");
    return out;
}

fn render(template: String, context: Map) -> String {
    let out = template;
    for key in keys(context) {
        let val = context[key];
        let start_if = "{{#if " + key + "}}";
        let end_if = "{{/if}}";
        if contains(out, start_if) and contains(out, end_if) {
            let parts = split(out, start_if);
            if len(parts) >= 2 {
                let tail_parts = split(parts[1], end_if);
                if len(tail_parts) >= 2 {
                    let mid = "";
                    if val {
                        mid = tail_parts[0];
                    }
                    out = parts[0] + mid + tail_parts[1];
                }
            }
        }
        let start_each = "{{#each " + key + "}}";
        let end_each = "{{/each}}";
        if contains(out, start_each) and contains(out, end_each) {
            let parts = split(out, start_each);
            if len(parts) >= 2 {
                let tail_parts = split(parts[1], end_each);
                if len(tail_parts) >= 2 {
                    let body = tail_parts[0];
                    let rendered_items = "";
                    for item in val {
                        rendered_items = rendered_items + replace(body, "{{this}}", str(item));
                    }
                    out = parts[0] + rendered_items + tail_parts[1];
                }
            }
        }
        let s = str(val);
        out = replace(out, "{{{" + key + "}}}", s);
        out = replace(out, "{{" + key + "}}", escape_html(s));
    }
    return out;
}
