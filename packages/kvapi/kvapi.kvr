fn response(body: Any, status: Number, content_type: String) -> Map {
    return {"status":status,"content_type":content_type,"body":body};
}
fn response_with_headers(body: Any, status: Number, content_type: String, headers: Map) -> Map {
    return {"status":status,"content_type":content_type,"body":body,"headers":headers};
}
fn request_header(request: Map, name: String) -> Any { return get(request["headers"],name); }
fn ok(data: Any) -> Map { return response(json_stringify(data),200,"application/json; charset=utf-8"); }
fn created(data: Any) -> Map { return response(json_stringify(data),201,"application/json; charset=utf-8"); }
fn bad_request(data: Any) -> Map { return response(json_stringify(data),400,"application/json; charset=utf-8"); }
fn unauthorized(data: Any) -> Map { return response(json_stringify(data),401,"application/json; charset=utf-8"); }
fn forbidden(data: Any) -> Map { return response(json_stringify(data),403,"application/json; charset=utf-8"); }
fn not_found(data: Any) -> Map { return response(json_stringify(data),404,"application/json; charset=utf-8"); }
fn server_error(data: Any) -> Map { return response(json_stringify(data),500,"application/json; charset=utf-8"); }
fn body_json(request: Map) -> Any { return json_parse(request["body"]); }
fn path_param(request: Map, name: String) -> Any { return get(request["params"],name); }
fn query_param(request: Map, name: String) -> Any { return get(request["query"],name); }
