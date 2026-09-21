fn html_page(title: String, body: String) -> String {
    return "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>" + title + "</title></head><body>" + body + "</body></html>";
}
fn heading(text: String) -> String { return "<h1>" + text + "</h1>"; }
fn paragraph(text: String) -> String { return "<p>" + text + "</p>"; }
fn link(href: String, text: String) -> String { return "<a href=\"" + href + "\">" + text + "</a>"; }

fn response(body: Any, status: Number, content_type: String) -> Map {
    return {"status":status, "content_type":content_type, "body":body};
}
fn json_response(data: Any) -> Map {
    return response(json_stringify(data), 200, "application/json; charset=utf-8");
}
fn json_status(data: Any, status: Number) -> Map {
    return response(json_stringify(data), status, "application/json; charset=utf-8");
}
fn text_response(text: String) -> Map {
    return response(text, 200, "text/plain; charset=utf-8");
}
fn html_response(html: String) -> Map {
    return response(html, 200, "text/html; charset=utf-8");
}
fn param(request: Map, name: String) -> Any { return get(request["params"], name); }
fn query_param(request: Map, name: String) -> Any { return get(request["query"], name); }
fn json_body(request: Map) -> Any { return json_parse(request["body"]); }
