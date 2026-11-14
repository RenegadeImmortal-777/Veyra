// HTTP servers: static directory serving, file-backed routes and the dynamic
// Kovrek app server.
//
// Extracted from `main.rs` without behavior change; see docs/ENGINE-SPLIT.md.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value as JsonValue;
use tiny_http::{Header, Response, Server, StatusCode};

use crate::pipeline::{load_source, prepare_vm};
use crate::registry::{safe_package_relative_path, unpack_bundle_to_dir};
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
    println!("Kovrek web server: http://127.0.0.1:{port}");
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
    println!("Kovrek API server: http://127.0.0.1:{port}");
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

pub(crate) fn serve_kovrek_app(
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
    println!("Kovrek dynamic app: http://127.0.0.1:{port}");
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
                Err(e) => Response::from_string(format!("Kovrek handler error: {e}"))
                    .with_status_code(StatusCode(500)),
            }
        } else {
            Response::from_string("404 Not Found").with_status_code(StatusCode(404))
        };
        req.respond(response).map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub(crate) fn serve_registry(dir: &Path, port: u16) -> Result<(), String> {
    let server = Server::http(format!("0.0.0.0:{port}")).map_err(|e| e.to_string())?;
    println!("Kovrek Package Registry Server: http://127.0.0.1:{port}");
    println!("Root {}", dir.display());
    for mut req in server.incoming_requests() {
        let method = req.method().as_str().to_string();
        let path = req.url().split('?').next().unwrap_or("/").to_string();
        let response = if method == "POST" && (path == "/publish" || path == "/registry/publish") {
            let mut body = String::new();
            let resp = match req.as_reader().read_to_string(&mut body) {
                Ok(_) => match serde_json::from_str::<JsonValue>(&body) {
                    Ok(bundle) => {
                        let name = bundle.get("name").and_then(JsonValue::as_str).unwrap_or("");
                        let version = bundle
                            .get("version")
                            .and_then(JsonValue::as_str)
                            .unwrap_or("0.1.0");
                        let entry = bundle
                            .get("entry")
                            .and_then(JsonValue::as_str)
                            .unwrap_or("src/main.kvr");
                        let sha256 = bundle
                            .get("sha256")
                            .and_then(JsonValue::as_str)
                            .unwrap_or("");
                        if name.is_empty() || name.contains('/') || name.contains('\\') {
                            Response::from_string("invalid package name")
                                .with_status_code(StatusCode(400))
                        } else {
                            let pkg_dir = dir.join("packages").join(name);
                            match unpack_bundle_to_dir(&bundle, &pkg_dir) {
                                Ok(()) => {
                                    let bundles_dir = dir.join("bundles");
                                    let _ = fs::create_dir_all(&bundles_dir);
                                    let _ =
                                        fs::write(bundles_dir.join(format!("{name}.kvpkg")), &body);
                                    let index_path = dir.join("registry").join("index.json");
                                    if let Some(parent) = index_path.parent() {
                                        let _ = fs::create_dir_all(parent);
                                    }
                                    let mut index: JsonValue = fs::read_to_string(&index_path)
                                        .ok()
                                        .and_then(|t| serde_json::from_str(&t).ok())
                                        .unwrap_or_else(|| {
                                            serde_json::json!({"registry_version":1,"packages":{}})
                                        });
                                    if !index.get("packages").is_some_and(JsonValue::is_object) {
                                        index["packages"] =
                                            JsonValue::Object(serde_json::Map::new());
                                    }
                                    let mut meta = serde_json::json!({
                                        "version": version,
                                        "entry": entry,
                                        "source": format!("../packages/{name}"),
                                        "bundle_path": format!("/bundles/{name}.kvpkg"),
                                        "sha256": sha256
                                    });
                                    if let Some(deps) = bundle.get("dependencies") {
                                        meta["dependencies"] = deps.clone();
                                    }
                                    if let Some(nat) = bundle.get("native").filter(|v| !v.is_null())
                                    {
                                        meta["native"] = nat.clone();
                                    }
                                    index["packages"][name] = meta;
                                    match serde_json::to_string_pretty(&index) {
                                        Ok(idx_text) => {
                                            if let Err(e) = fs::write(&index_path, idx_text) {
                                                Response::from_string(format!("write failed: {e}"))
                                                    .with_status_code(StatusCode(500))
                                            } else {
                                                let ok_json = serde_json::json!({
                                                    "published": name,
                                                    "version": version,
                                                    "sha256": sha256
                                                });
                                                Response::from_string(ok_json.to_string())
                                            }
                                        }
                                        Err(e) => Response::from_string(e.to_string())
                                            .with_status_code(StatusCode(500)),
                                    }
                                }
                                Err(e) => Response::from_string(format!("bundle error: {e}"))
                                    .with_status_code(StatusCode(400)),
                            }
                        }
                    }
                    Err(e) => Response::from_string(format!("invalid JSON: {e}"))
                        .with_status_code(StatusCode(400)),
                },
                Err(e) => Response::from_string(e.to_string()).with_status_code(StatusCode(400)),
            };
            resp
        } else if method == "GET"
            && (path == "/" || path == "/index.json" || path == "/registry/index.json")
        {
            let p1 = dir.join("registry").join("index.json");
            let p2 = dir.join("index.json");
            let chosen = if p1.is_file() { p1 } else { p2 };
            match fs::read(&chosen) {
                Ok(data) => {
                    let mut r = Response::from_data(data);
                    if let Ok(h) = Header::from_bytes(
                        &b"Content-Type"[..],
                        &b"application/json; charset=utf-8"[..],
                    ) {
                        r.add_header(h);
                    }
                    r
                }
                Err(_) => Response::from_string("404 Registry index not found")
                    .with_status_code(StatusCode(404)),
            }
        } else if let Some(sp) = resolve_static_path(dir, req.url()) {
            match fs::read(&sp) {
                Ok(data) => Response::from_data(data),
                Err(_) => Response::from_string("500 Read error").with_status_code(StatusCode(500)),
            }
        } else {
            Response::from_string("404 Not Found").with_status_code(StatusCode(404))
        };
        req.respond(response).map_err(|e| e.to_string())?;
    }
    Ok(())
}

// =========================================================================
// Persistent Reactive Notebook Runtime & Server (`kovrek notebook`)
// =========================================================================

#[derive(Clone, Debug)]
pub(crate) struct NotebookCellResult {
    pub(crate) cell_id: String,
    pub(crate) source: String,
    pub(crate) stdout: String,
    pub(crate) value: Option<String>,
    pub(crate) error: Option<String>,
    pub(crate) exec_count: usize,
    pub(crate) defs: Vec<String>,
    pub(crate) refs: Vec<String>,
    pub(crate) cascade_updated: Vec<String>,
}

pub(crate) struct NotebookSession {
    vars: std::collections::HashMap<String, Val>,
    funcs: std::sync::Arc<std::collections::HashMap<String, crate::bytecode::FunctionDef>>,
    classes: std::sync::Arc<std::collections::HashMap<String, crate::bytecode::ClassDef>>,
    objects: std::sync::Arc<
        std::sync::Mutex<std::collections::HashMap<u64, crate::bytecode::ObjectData>>,
    >,
    class_objects: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, u64>>>,
    pub(crate) cells: std::collections::BTreeMap<String, NotebookCellResult>,
    cell_order: Vec<String>,
    cell_deps: std::collections::BTreeMap<
        String,
        (
            std::collections::BTreeSet<String>,
            std::collections::BTreeSet<String>,
            std::collections::BTreeSet<String>,
        ),
    >,
    exec_counter: usize,
    storage_file: Option<PathBuf>,
}

impl NotebookSession {
    pub(crate) fn new(storage_file: Option<PathBuf>) -> Self {
        Self {
            vars: std::collections::HashMap::new(),
            funcs: std::sync::Arc::new(std::collections::HashMap::new()),
            classes: std::sync::Arc::new(std::collections::HashMap::new()),
            objects: std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            class_objects: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            cells: std::collections::BTreeMap::new(),
            cell_order: Vec::new(),
            cell_deps: std::collections::BTreeMap::new(),
            exec_counter: 0,
            storage_file,
        }
    }

    pub(crate) fn reset(&mut self) {
        self.vars.clear();
        self.funcs = std::sync::Arc::new(std::collections::HashMap::new());
        self.classes = std::sync::Arc::new(std::collections::HashMap::new());
        self.objects = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        self.class_objects =
            std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        self.cells.clear();
        self.cell_order.clear();
        self.cell_deps.clear();
        self.exec_counter = 0;
        self.persist();
    }

    fn persist(&self) {
        let Some(ref path) = self.storage_file else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let items: Vec<JsonValue> = self
            .cells
            .values()
            .map(|c| {
                serde_json::json!({
                    "id": c.cell_id,
                    "source": c.source,
                    "stdout": c.stdout,
                    "value": c.value,
                    "error": c.error,
                    "exec_count": c.exec_count,
                    "defs": c.defs,
                    "refs": c.refs,
                })
            })
            .collect();
        if let Ok(raw) = serde_json::to_string_pretty(&serde_json::json!({ "cells": items })) {
            let _ = fs::write(path, raw);
        }
    }

    fn compute_dag_over(
        order_ids: &[String],
        deps_map: &std::collections::BTreeMap<
            String,
            (
                std::collections::BTreeSet<String>,
                std::collections::BTreeSet<String>,
                std::collections::BTreeSet<String>,
            ),
        >,
    ) -> (
        std::collections::BTreeMap<String, Vec<String>>,
        Vec<String>,
        Vec<String>,
    ) {
        let mut adj: std::collections::BTreeMap<String, std::collections::BTreeSet<String>> =
            std::collections::BTreeMap::new();
        let mut indeg: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        for id in order_ids {
            adj.entry(id.clone()).or_default();
            indeg.entry(id.clone()).or_insert(0);
        }
        for i in 0..order_ids.len() {
            for j in 0..order_ids.len() {
                if i == j {
                    continue;
                }
                let u = &order_ids[i];
                let v = &order_ids[j];
                let Some((u_defs, u_assigns, _)) = deps_map.get(u) else {
                    continue;
                };
                let Some((v_defs, v_assigns, v_refs)) = deps_map.get(v) else {
                    continue;
                };
                let def_edge = u_defs
                    .iter()
                    .any(|s| v_refs.contains(s) || v_assigns.contains(s));
                let assign_to_reader = u_assigns
                    .iter()
                    .any(|s| v_refs.contains(s) && !v_defs.contains(s) && !v_assigns.contains(s));
                if def_edge || assign_to_reader {
                    if adj.get_mut(u).is_some_and(|set| set.insert(v.clone())) {
                        *indeg.entry(v.clone()).or_insert(0) += 1;
                    }
                }
            }
        }

        let order_rank: std::collections::HashMap<&str, usize> = order_ids
            .iter()
            .enumerate()
            .map(|(idx, s)| (s.as_str(), idx))
            .collect();
        let mut ready: Vec<String> = order_ids
            .iter()
            .filter(|id| indeg.get(*id).copied().unwrap_or(0) == 0)
            .cloned()
            .collect();
        let mut topo = Vec::with_capacity(order_ids.len());
        while !ready.is_empty() {
            let cur = ready.remove(0);
            topo.push(cur.clone());
            if let Some(nbrs) = adj.get(&cur) {
                for nxt in nbrs {
                    if let Some(d) = indeg.get_mut(nxt) {
                        *d = d.saturating_sub(1);
                        if *d == 0 {
                            ready.push(nxt.clone());
                        }
                    }
                }
                ready.sort_by_key(|id| order_rank.get(id.as_str()).copied().unwrap_or(usize::MAX));
            }
        }
        let cycle_cells: Vec<String> = order_ids
            .iter()
            .filter(|id| !topo.contains(id))
            .cloned()
            .collect();
        let adj_vec = adj
            .into_iter()
            .map(|(k, v)| (k, v.into_iter().collect()))
            .collect();
        (adj_vec, topo, cycle_cells)
    }

    fn execute_single_cell(
        &mut self,
        _cell_id: &str,
        source: &str,
    ) -> (String, Option<String>, Option<String>) {
        self.eval_snippet(source, true)
    }

    /// Evaluates a stateful Kovrek snippet against persistent session globals,
    /// functions, classes, and live object instances.
    fn eval_snippet(
        &mut self,
        source: &str,
        capture_stdout: bool,
    ) -> (String, Option<String>, Option<String>) {
        let trimmed = source.trim();
        let mut is_expr_fallback = false;
        let predeclared: Vec<String> = self
            .vars
            .keys()
            .chain(self.funcs.keys())
            .chain(self.classes.keys())
            .cloned()
            .collect();
        let compiled = if trimmed.is_empty() {
            Ok((Vec::new(), Vec::new()))
        } else {
            match crate::pipeline::compile_notebook_cell_with_globals(source, &predeclared) {
                Ok((code, dbg)) => Ok((code, dbg.local_names)),
                Err(stmt_err) => match crate::pipeline::compile_eval_expr(trimmed) {
                    Ok(code) => {
                        is_expr_fallback = true;
                        Ok((code, Vec::new()))
                    }
                    Err(_) => Err(stmt_err),
                },
            }
        };

        match compiled {
            Err(e) => (String::new(), None, Some(e)),
            Ok((code, local_names)) => {
                if code.is_empty() {
                    (String::new(), None, None)
                } else {
                    let mut vm = crate::vm::Vm::new(code);
                    vm.vars = self.vars.clone();
                    vm.funcs = self.funcs.clone();
                    vm.classes = self.classes.clone();
                    vm.objects = self.objects.clone();
                    vm.class_objects = self.class_objects.clone();
                    for (slot, name) in local_names.iter().enumerate() {
                        if let Some(val) = vm.vars.get(name).cloned()
                            && slot < vm.locals.len()
                        {
                            vm.locals[slot] = val;
                            vm.local_defined[slot] = true;
                        }
                    }

                    let (res, captured) = if capture_stdout {
                        crate::runtime::with_print_capture(|| vm.execute())
                    } else {
                        (vm.execute(), String::new())
                    };
                    for (slot, name) in local_names.iter().enumerate() {
                        if !name.starts_with("__")
                            && vm.local_defined.get(slot).copied().unwrap_or(false)
                            && let Some(val) = vm.locals.get(slot)
                        {
                            vm.vars.insert(name.clone(), val.clone());
                        }
                    }
                    self.vars = vm.vars;
                    self.funcs = vm.funcs;
                    self.classes = vm.classes;
                    self.objects = vm.objects;
                    self.class_objects = vm.class_objects;
                    match res {
                        Ok(v) => {
                            let val_str = if is_expr_fallback || !matches!(v, Val::Null) {
                                Some(v.show())
                            } else {
                                None
                            };
                            (captured, val_str, None)
                        }
                        Err(err) => (captured, None, Some(err)),
                    }
                }
            }
        }
    }

    /// Evaluates one interactive REPL/terminal input against persistent session state.
    pub(crate) fn eval_repl(&mut self, source: &str) -> Result<Option<String>, String> {
        let (_out, val, err) = self.eval_snippet(source, false);
        match err {
            Some(e) => Err(e),
            None => Ok(val),
        }
    }

    /// Lists user-visible session globals sorted by identifier name.
    pub(crate) fn session_vars(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self
            .vars
            .iter()
            .filter(|(k, _)| !k.starts_with("__"))
            .map(|(k, v)| (k.clone(), v.show()))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    pub(crate) fn eval_cell(&mut self, cell_id: &str, source: &str) -> NotebookCellResult {
        self.exec_counter += 1;
        let exec_count = self.exec_counter;
        let (defs_set, assigns_set, refs_set) = crate::pipeline::analyze_cell_deps(source);
        let defs_vec: Vec<String> = defs_set.iter().cloned().collect();
        let refs_vec: Vec<String> = refs_set.iter().cloned().collect();

        let is_new = !self.cell_order.iter().any(|c| c == cell_id);
        let mut next_order = self.cell_order.clone();
        if is_new {
            next_order.push(cell_id.to_string());
        }
        let mut next_deps = self.cell_deps.clone();
        next_deps.insert(cell_id.to_string(), (defs_set, assigns_set, refs_set));

        let (adj, topo, cycle_cells) = Self::compute_dag_over(&next_order, &next_deps);
        if !cycle_cells.is_empty() {
            let err_msg = format!(
                "NotebookCycleError: cyclic dependency among cells {:?}",
                cycle_cells
            );
            self.cell_order = next_order;
            self.cell_deps = next_deps;
            let result = NotebookCellResult {
                cell_id: cell_id.to_string(),
                source: source.to_string(),
                stdout: String::new(),
                value: None,
                error: Some(err_msg),
                exec_count,
                defs: defs_vec,
                refs: refs_vec,
                cascade_updated: Vec::new(),
            };
            self.cells.insert(cell_id.to_string(), result.clone());
            self.persist();
            return result;
        }

        self.cell_order = next_order;
        self.cell_deps = next_deps;
        let (stdout, value, error) = self.execute_single_cell(cell_id, source);

        // Find all strict downstream cells in topological order and re-execute when cell succeeded.
        let mut downstream_set = std::collections::HashSet::new();
        let mut queue = std::collections::VecDeque::new();
        if error.is_none() {
            queue.push_back(cell_id.to_string());
            while let Some(cur) = queue.pop_front() {
                if let Some(nbrs) = adj.get(&cur) {
                    for nxt in nbrs {
                        if nxt != cell_id && downstream_set.insert(nxt.clone()) {
                            queue.push_back(nxt.clone());
                        }
                    }
                }
            }
        }

        let mut cascade_updated = Vec::new();
        for dep_id in topo {
            if !downstream_set.contains(&dep_id) {
                continue;
            }
            let Some(prev) = self.cells.get(&dep_id).cloned() else {
                continue;
            };
            self.exec_counter += 1;
            let c_exec = self.exec_counter;
            let (c_out, c_val, c_err) = self.execute_single_cell(&dep_id, &prev.source);
            let updated = NotebookCellResult {
                cell_id: dep_id.clone(),
                source: prev.source,
                stdout: c_out,
                value: c_val,
                error: c_err,
                exec_count: c_exec,
                defs: prev.defs,
                refs: prev.refs,
                cascade_updated: Vec::new(),
            };
            self.cells.insert(dep_id.clone(), updated);
            cascade_updated.push(dep_id);
        }

        let result = NotebookCellResult {
            cell_id: cell_id.to_string(),
            source: source.to_string(),
            stdout,
            value,
            error,
            exec_count,
            defs: defs_vec,
            refs: refs_vec,
            cascade_updated,
        };
        self.cells.insert(cell_id.to_string(), result.clone());
        self.persist();
        result
    }

    pub(crate) fn dag_info(&self) -> JsonValue {
        let (adj, topo, cycle_cells) = Self::compute_dag_over(&self.cell_order, &self.cell_deps);
        let nodes: Vec<JsonValue> = self
            .cell_order
            .iter()
            .map(|id| {
                let (defs, assigns, refs) = self.cell_deps.get(id).cloned().unwrap_or_default();
                serde_json::json!({
                    "id": id,
                    "defs": defs.into_iter().collect::<Vec<_>>(),
                    "assigns": assigns.into_iter().collect::<Vec<_>>(),
                    "refs": refs.into_iter().collect::<Vec<_>>(),
                })
            })
            .collect();
        let mut edges = Vec::new();
        for (from, tos) in adj {
            for to in tos {
                edges.push(serde_json::json!({ "from": from, "to": to }));
            }
        }
        serde_json::json!({
            "nodes": nodes,
            "edges": edges,
            "order": topo,
            "has_cycle": !cycle_cells.is_empty(),
            "cycle_cells": cycle_cells,
        })
    }

    pub(crate) fn export_script(&self) -> String {
        let (_, topo, _) = Self::compute_dag_over(&self.cell_order, &self.cell_deps);
        let ordered = if topo.is_empty() {
            self.cell_order.clone()
        } else {
            topo
        };
        let mut out = String::new();
        for id in ordered {
            if let Some(cell) = self.cells.get(&id) {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&format!("// --- cell: {} ---\n", cell.cell_id));
                out.push_str(cell.source.trim_end());
                out.push('\n');
            }
        }
        out
    }
}

pub(crate) fn handle_notebook_api_request(
    session: &std::sync::Arc<std::sync::Mutex<NotebookSession>>,
    method: &str,
    url: &str,
    body: &str,
) -> (u16, &'static str, String) {
    match (method, url) {
        ("GET", "/api/notebook/state") => {
            let Ok(guard) = session.lock() else {
                return (500, "text/plain", "500 Session error".into());
            };
            let items: Vec<JsonValue> = guard
                .cells
                .values()
                .map(|c| {
                    serde_json::json!({
                        "id": c.cell_id,
                        "source": c.source,
                        "stdout": c.stdout,
                        "value": c.value,
                        "error": c.error,
                        "exec_count": c.exec_count,
                        "defs": c.defs,
                        "refs": c.refs,
                    })
                })
                .collect();
            (
                200,
                "application/json",
                serde_json::to_string(&serde_json::json!({ "cells": items })).unwrap_or_default(),
            )
        }
        ("GET", "/api/notebook/dag") => {
            let Ok(guard) = session.lock() else {
                return (500, "text/plain", "500 Session error".into());
            };
            (
                200,
                "application/json",
                serde_json::to_string(&guard.dag_info()).unwrap_or_default(),
            )
        }
        ("GET", "/api/notebook/export") => {
            let Ok(guard) = session.lock() else {
                return (500, "text/plain", "500 Session error".into());
            };
            (200, "text/plain; charset=utf-8", guard.export_script())
        }
        ("POST", "/api/notebook/run") => {
            let parsed: JsonValue = serde_json::from_str(body).unwrap_or_default();
            let cid = parsed
                .get("id")
                .and_then(JsonValue::as_str)
                .unwrap_or("cell1");
            let src = parsed
                .get("source")
                .and_then(JsonValue::as_str)
                .unwrap_or("");
            let Ok(mut guard) = session.lock() else {
                return (500, "text/plain", "500 Session error".into());
            };
            let res = guard.eval_cell(cid, src);
            (
                200,
                "application/json",
                serde_json::to_string(&serde_json::json!({
                    "id": res.cell_id,
                    "stdout": res.stdout,
                    "value": res.value,
                    "error": res.error,
                    "exec_count": res.exec_count,
                    "defs": res.defs,
                    "refs": res.refs,
                    "cascade_updated": res.cascade_updated,
                }))
                .unwrap_or_default(),
            )
        }
        ("POST", "/api/notebook/reset") => {
            if let Ok(mut guard) = session.lock() {
                guard.reset();
            }
            (200, "application/json", r#"{"ok":true}"#.into())
        }
        _ => (404, "text/plain", "404 Not Found".into()),
    }
}

pub(crate) fn serve_notebook(dir: &Path, port: u16) -> Result<(), String> {
    let storage = dir.join(".kovrek-notebook.json");
    let session = std::sync::Arc::new(std::sync::Mutex::new(NotebookSession::new(Some(
        storage.clone(),
    ))));
    let server = Server::http(format!("0.0.0.0:{port}")).map_err(|e| e.to_string())?;
    println!("Kovrek Notebook server: http://127.0.0.1:{port}");
    println!("Workspace: {}", dir.display());

    const UI_HTML: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Kovrek Notebook</title>
<style>body{font-family:system-ui,sans-serif;max-width:900px;margin:2rem auto;background:#0f172a;color:#e2e8f0}
textarea{width:100%;height:90px;background:#1e293b;color:#f8fafc;border:1px solid #334155;padding:.6rem;font-family:monospace}
button{background:#38bdf8;color:#0f172a;border:0;padding:.45rem 1rem;border-radius:6px;font-weight:600;cursor:pointer;margin:.4rem .3rem 0 0}
pre{background:#020617;padding:.75rem;border-radius:6px;white-space:pre-wrap}</style></head>
<body><h1>Kovrek Reactive Notebook</h1>
<div><input id="cid" value="cell1" style="padding:.4rem;background:#1e293b;color:#f8fafc;border:1px solid #334155"/>
<button onclick="runCell()">Run Cell</button><button onclick="resetNb()">Reset Session</button></div>
<textarea id="src" placeholder="let x = 21 * 2;&#10;print(x);"></textarea>
<pre id="out">Ready.</pre>
<script>
async function runCell(){const r=await fetch('/api/notebook/run',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({id:cid.value,source:src.value})});const d=await r.json();out.textContent=JSON.stringify(d,null,2);}
async function resetNb(){await fetch('/api/notebook/reset',{method:'POST'});out.textContent='Session reset.';}
</script></body></html>"#;

    for mut req in server.incoming_requests() {
        let method = req.method().as_str().to_string();
        let url = req.url().split('?').next().unwrap_or("/").to_string();
        let response = if method == "GET" && url == "/" {
            let mut r = Response::from_string(UI_HTML);
            if let Ok(h) =
                Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..])
            {
                r.add_header(h);
            }
            r
        } else {
            let mut body = String::new();
            if method == "POST" {
                let _ = req.as_reader().read_to_string(&mut body);
            }
            let (status, ctype, text) = handle_notebook_api_request(&session, &method, &url, &body);
            let mut r = Response::from_string(text).with_status_code(StatusCode(status));
            if let Ok(h) = Header::from_bytes(&b"Content-Type"[..], ctype.as_bytes()) {
                r.add_header(h);
            }
            r
        };
        req.respond(response).map_err(|e| e.to_string())?;
    }
    Ok(())
}
