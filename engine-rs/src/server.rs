// HTTP servers: static directory serving, file-backed routes and the dynamic
// Veyra app server.
//
// Extracted from `main.rs` without behavior change; see docs/ENGINE-SPLIT.md.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value as JsonValue;
use tiny_http::{Header, Response, Server, StatusCode};

use crate::pipeline::{load_source, prepare_vm};
use crate::registry::safe_package_relative_path;
use crate::value::{Val, map_lookup, val_to_json};

pub(crate) fn resolve_static_path(dir: &Path, request_url: &str) -> Option<PathBuf> {
    let raw = request_url.split('?').next().unwrap_or("/");
    let rel = raw.trim_start_matches('/');
    let rel_path = if rel.is_empty() {
        PathBuf::from("index.html")
    } else {
        safe_package_relative_path(rel).ok()?
    };
    let mut candidate = dir.join(rel_path);
    if candidate.is_dir() {
        candidate = candidate.join("index.html");
    }
    let canon_dir = dir.canonicalize().ok()?;
    let canon_file = candidate.canonicalize().ok()?;
    if canon_file.starts_with(&canon_dir) && canon_file.is_file() {
        Some(canon_file)
    } else {
        None
    }
}

pub(crate) fn serve_directory(dir: &Path, port: u16) -> Result<(), String> {
    let server = Server::http(format!("0.0.0.0:{port}")).map_err(|e| e.to_string())?;
    println!("Veyra web server: http://127.0.0.1:{port}");
    println!("Serving {}", dir.display());
    for req in server.incoming_requests() {
        let response = if let Some(path) = resolve_static_path(dir, req.url()) {
            let data = fs::read(&path).map_err(|e| e.to_string())?;
            let mut r = Response::from_data(data);
            let content_type = match path.extension().and_then(|x| x.to_str()).unwrap_or("") {
                "html" => "text/html; charset=utf-8",
                "css" => "text/css; charset=utf-8",
                "js" => "application/javascript; charset=utf-8",
                "json" => "application/json; charset=utf-8",
                "png" => "image/png",
                "jpg" | "jpeg" => "image/jpeg",
                "svg" => "image/svg+xml",
                _ => "application/octet-stream",
            };
            if let Ok(h) = Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes()) {
                r.add_header(h);
            }
            r
        } else {
            Response::from_string("404 Not Found").with_status_code(StatusCode(404))
        };
        req.respond(response).map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub(crate) fn serve_routes(routes_file: &Path, port: u16) -> Result<(), String> {
    let config: JsonValue =
        serde_json::from_str(&fs::read_to_string(routes_file).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let routes = config
        .get("routes")
        .and_then(JsonValue::as_object)
        .ok_or("web route file requires a 'routes' object")?;
    let server = Server::http(format!("0.0.0.0:{port}")).map_err(|e| e.to_string())?;
    println!("Veyra API server: http://127.0.0.1:{port}");
    println!("Routes: {}", routes.len());
    for mut req in server.incoming_requests() {
        let path = req.url().split('?').next().unwrap_or("/");
        let method = req.method().as_str();
        let method_key = format!("{method} {path}");
        let route = routes.get(&method_key).or_else(|| routes.get(path));
        let response = if let Some(route) = route {
            let status = route
                .get("status")
                .and_then(JsonValue::as_u64)
                .unwrap_or(200) as u16;
            let content_type = route
                .get("content_type")
                .and_then(JsonValue::as_str)
                .unwrap_or_else(|| {
                    if route.get("json").is_some() {
                        "application/json; charset=utf-8"
                    } else {
                        "text/plain; charset=utf-8"
                    }
                });
            let body = if route
                .get("echo")
                .and_then(JsonValue::as_bool)
                .unwrap_or(false)
            {
                let mut text = String::new();
                req.as_reader()
                    .read_to_string(&mut text)
                    .map_err(|e| e.to_string())?;
                text
            } else if let Some(j) = route.get("json") {
                serde_json::to_string(j).map_err(|e| e.to_string())?
            } else {
                route
                    .get("body")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("")
                    .to_string()
            };
            let mut r = Response::from_string(body).with_status_code(StatusCode(status));
            if let Ok(h) = Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes()) {
                r.add_header(h);
            }
            r
        } else {
            Response::from_string("404 Not Found").with_status_code(StatusCode(404))
        };
        req.respond(response).map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub(crate) fn match_route_pattern(pattern: &str, path: &str) -> Option<Vec<(String, String)>> {
    let p: Vec<&str> = pattern
        .trim_matches('/')
        .split('/')
        .filter(|x| !x.is_empty())
        .collect();
    let a: Vec<&str> = path
        .trim_matches('/')
        .split('/')
        .filter(|x| !x.is_empty())
        .collect();
    if p.len() != a.len() {
        return None;
    }
    let mut params = vec![];
    for (expected, actual) in p.iter().zip(a.iter()) {
        if expected.starts_with('{') && expected.ends_with('}') && expected.len() > 2 {
            params.push((
                expected[1..expected.len() - 1].to_string(),
                (*actual).to_string(),
            ));
        } else if expected != actual {
            return None;
        }
    }
    Some(params)
}

pub(crate) fn query_params(url: &str) -> Val {
    let mut pairs = vec![];
    if let Some(query) = url.split_once('?').map(|(_, q)| q) {
        for item in query.split('&').filter(|x| !x.is_empty()) {
            let (key, value) = item.split_once('=').unwrap_or((item, ""));
            pairs.push((Val::Str(key.to_string()), Val::Str(value.to_string())));
        }
    }
    Val::Map(pairs)
}

pub(crate) fn serve_veyra_app(
    app_file: &Path,
    routes_file: &Path,
    port: u16,
) -> Result<(), String> {
    let src = load_source(app_file.to_string_lossy().as_ref())?;
    let vm = prepare_vm(&src)?;
    let config: JsonValue =
        serde_json::from_str(&fs::read_to_string(routes_file).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let routes = config
        .get("routes")
        .and_then(JsonValue::as_object)
        .ok_or("app routes file requires a 'routes' object")?;
    let server = Server::http(format!("0.0.0.0:{port}")).map_err(|e| e.to_string())?;
    println!("Veyra dynamic app: http://127.0.0.1:{port}");
    println!("Handlers: {}", routes.len());
    for mut req in server.incoming_requests() {
        let full_url = req.url().to_string();
        let path = full_url.split('?').next().unwrap_or("/").to_string();
        let method = req.method().as_str().to_string();
        let key = format!("{method} {path}");
        let mut route_params: Vec<(String, String)> = vec![];
        let mut handler = routes
            .get(&key)
            .or_else(|| routes.get(&path))
            .and_then(JsonValue::as_str)
            .map(str::to_string);
        if handler.is_none() {
            for (route_key, value) in routes {
                let (route_method, pattern) = route_key
                    .split_once(' ')
                    .map(|(m, p)| (Some(m), p))
                    .unwrap_or((None, route_key.as_str()));
                if route_method.is_some() && route_method != Some(method.as_str()) {
                    continue;
                }
                if let Some(params) = match_route_pattern(pattern, &path) {
                    if let Some(name) = value.as_str() {
                        handler = Some(name.to_string());
                        route_params = params;
                        break;
                    }
                }
            }
        }
        let response = if let Some(handler) = handler {
            let mut body = String::new();
            req.as_reader()
                .read_to_string(&mut body)
                .map_err(|e| e.to_string())?;
            let params = Val::Map(
                route_params
                    .iter()
                    .map(|(k, v)| (Val::Str(k.clone()), Val::Str(v.clone())))
                    .collect(),
            );
            let query = query_params(&full_url);
            let headers = Val::Map(
                req.headers()
                    .iter()
                    .map(|h| {
                        (
                            Val::Str(h.field.as_str().to_string()),
                            Val::Str(h.value.as_str().to_string()),
                        )
                    })
                    .collect(),
            );
            let request = Val::Map(vec![
                (Val::Str("method".into()), Val::Str(method)),
                (Val::Str("path".into()), Val::Str(path)),
                (Val::Str("url".into()), Val::Str(full_url)),
                (Val::Str("body".into()), Val::Str(body)),
                (Val::Str("params".into()), params),
                (Val::Str("query".into()), query),
                (Val::Str("headers".into()), headers),
            ]);
            match vm.invoke(&handler, vec![request]) {
                Ok(value) => {
                    let (status, content_type, body, response_headers) = match value {
                        Val::Map(pairs) => {
                            let status = map_lookup(&pairs, &Val::Str("status".into()))
                                .and_then(|v| {
                                    if let Val::Num(n) = v {
                                        Some(*n as u16)
                                    } else {
                                        None
                                    }
                                })
                                .unwrap_or(200);
                            let content_type = map_lookup(&pairs, &Val::Str("content_type".into()))
                                .and_then(|v| {
                                    if let Val::Str(s) = v {
                                        Some(s.clone())
                                    } else {
                                        None
                                    }
                                })
                                .unwrap_or_else(|| "application/json; charset=utf-8".into());
                            let body = map_lookup(&pairs, &Val::Str("body".into()))
                                .cloned()
                                .unwrap_or(Val::Null);
                            let text = match body {
                                Val::Str(s) => s,
                                other => serde_json::to_string(&val_to_json(&other))
                                    .map_err(|e| e.to_string())?,
                            };
                            let headers = match map_lookup(&pairs, &Val::Str("headers".into())) {
                                Some(Val::Map(items)) => items
                                    .iter()
                                    .filter_map(|(k, v)| match (k, v) {
                                        (Val::Str(name), Val::Str(value)) => {
                                            Some((name.clone(), value.clone()))
                                        }
                                        _ => None,
                                    })
                                    .collect::<Vec<_>>(),
                                _ => vec![],
                            };
                            (status, content_type, text, headers)
                        }
                        Val::Str(s) => (200, "text/plain; charset=utf-8".into(), s, vec![]),
                        other => (
                            200,
                            "application/json; charset=utf-8".into(),
                            serde_json::to_string(&val_to_json(&other))
                                .map_err(|e| e.to_string())?,
                            vec![],
                        ),
                    };
                    let mut r = Response::from_string(body).with_status_code(StatusCode(status));
                    if let Ok(h) = Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes())
                    {
                        r.add_header(h);
                    }
                    for (name, value) in response_headers {
                        if let Ok(h) = Header::from_bytes(name.as_bytes(), value.as_bytes()) {
                            r.add_header(h);
                        }
                    }
                    r
                }
                Err(e) => Response::from_string(format!("Veyra handler error: {e}"))
                    .with_status_code(StatusCode(500)),
            }
        } else {
            Response::from_string("404 Not Found").with_status_code(StatusCode(404))
        };
        req.respond(response).map_err(|e| e.to_string())?;
    }
    Ok(())
}
