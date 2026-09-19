fn html_page(title: String, body: String) -> String {
    return "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>" + title + "</title></head><body>" + body + "</body></html>";
}
fn heading(text: String) -> String { return "<h1>" + text + "</h1>"; }
fn paragraph(text: String) -> String { return "<p>" + text + "</p>"; }
fn link(href: String, text: String) -> String { return "<a href=\"" + href + "\">" + text + "</a>"; }
fn json_response(data: Any) -> String { return json_stringify(data); }
