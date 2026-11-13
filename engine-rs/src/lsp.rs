// Language server: completion, hover, definitions, references, rename, symbols,
// formatting and diagnostics over the lsp-server transport.
//
// Extracted from `main.rs` without behavior change; see docs/ENGINE-SPLIT.md.

use std::collections::HashMap;

use serde_json::Value as JsonValue;

use crate::VERSION;
use crate::ast::Stmt;
use crate::lexer::{lex, lex_with_lines};
use crate::parser::Parser;
use crate::pipeline::semantic_check_ast;
use crate::project::fmt_source;

pub(crate) fn lsp_word_at(text: &str, line: usize, character: usize) -> String {
    let Some(line_text) = text.lines().nth(line) else {
        return String::new();
    };
    let chars: Vec<char> = line_text.chars().collect();
    let pos = character.min(chars.len());
    let mut start = pos;
    while start > 0 && (chars[start - 1].is_ascii_alphanumeric() || chars[start - 1] == '_') {
        start -= 1;
    }
    let mut end = pos;
    while end < chars.len() && (chars[end].is_ascii_alphanumeric() || chars[end] == '_') {
        end += 1;
    }
    chars[start..end].iter().collect()
}

pub(crate) fn lsp_completion_items() -> JsonValue {
    let words = [
        "let",
        "const",
        "fn",
        "async",
        "class",
        "return",
        "if",
        "else",
        "while",
        "for",
        "in",
        "try",
        "catch",
        "finally",
        "throw",
        "with",
        "as",
        "match",
        "case",
        "break",
        "continue",
        "await",
        "yield",
        "gather",
        "true",
        "false",
        "null",
        "and",
        "or",
        "not",
        "print",
        "len",
        "str",
        "int",
        "float",
        "bool",
        "assert",
        "sum",
        "min",
        "max",
        "all",
        "any",
        "range",
        "keys",
        "values",
        "get",
        "type_of",
        "is_instance",
        "has_field",
        "append",
        "concat",
        "slice",
        "reverse",
        "sort",
        "unique",
        "enumerate",
        "zip",
        "map_call",
        "filter_call",
        "reduce_call",
        "spawn",
        "await_task",
        "task_done",
        "task_status",
        "await_timeout",
        "channel",
        "send",
        "recv",
        "try_recv",
        "http_get",
        "http_post",
        "json_parse",
        "json_stringify",
        "regex_findall",
        "sqlite_exec",
        "sqlite_query",
        "sha256",
        "vec_mean",
        "vec_std",
        "vec_median",
        "vec_percentile",
        "correlation",
        "matmul",
        "linreg_fit",
        "logreg_fit",
        "logreg_predict",
        "kmeans",
        "dense",
        "softmax",
        "tensor",
        "tensor_add",
        "tensor_sub",
        "tensor_mul",
        "tensor_div",
        "tensor_neg",
        "tensor_pow",
        "tensor_matmul",
        "tensor_softmax",
        "tensor_relu",
        "tensor_sigmoid",
        "tensor_tanh",
        "tensor_exp",
        "tensor_log",
        "tensor_reshape",
        "tensor_transpose",
        "tensor_scale",
        "tensor_cross_entropy",
        "tensor_layer_norm",
        "tensor_conv1d",
        "tensor_conv2d",
        "tensor_attention",
        "tensor_multihead_attention",
        "tensor_checkpoint_save",
        "tensor_checkpoint_load",
        "tensor_detach",
        "tensor_numel",
        "tensor_backward",
        "tensor_step",
        "tensor_adam_step",
        "tensor_reset_optimizer",
        "tensor_zero_grad",
        "csv_read",
        "csv_write",
        "table_select",
        "table_inner_join",
        "plot_svg",
        "ffi_available",
        "ffi_i32",
        "ffi_i64",
        "ffi_f64",
    ];
    JsonValue::Array(
        words.iter().map(|w| serde_json::json!({
            "label": w,
            "kind": if ["let","const","fn","class","return","if","else","while","for","in","try","catch","throw","match","case","break","continue","await"].contains(w) { 14 } else { 3 },
            "detail": "Veyra"
        })).collect()
    )
}

pub(crate) fn lsp_diagnostics(text: &str) -> Vec<JsonValue> {
    let has_imports = text
        .lines()
        .any(|line| line.trim_start().starts_with("use "));
    let filtered = text
        .lines()
        .map(|line| {
            if line.trim_start().starts_with("use ") {
                ""
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let result = match lex_with_lines(&filtered) {
        Err(e) => Err(e),
        Ok(tokens) => match Parser::new_with_lines(tokens).program() {
            Err(e) => Err(e),
            Ok(ast) => {
                if has_imports {
                    Ok(())
                } else {
                    semantic_check_ast(&ast)
                }
            }
        },
    };
    match result {
        Ok(()) => vec![],
        Err(message) => {
            let line = message
                .strip_prefix("line ")
                .and_then(|rest| rest.split(':').next())
                .and_then(|n| n.parse::<usize>().ok())
                .map(|n| n.saturating_sub(1))
                .unwrap_or(0);
            vec![serde_json::json!({
                "range": {
                    "start": {"line":line,"character":0},
                    "end": {"line":line,"character":1}
                },
                "severity": 1,
                "source": "veyra",
                "message": message
            })]
        }
    }
}

pub(crate) fn lsp_publish_diagnostics(
    connection: &lsp_server::Connection,
    uri: &str,
    text: &str,
) -> Result<(), String> {
    let notification = lsp_server::Notification {
        method: "textDocument/publishDiagnostics".into(),
        params: serde_json::json!({
            "uri": uri,
            "diagnostics": lsp_diagnostics(text)
        }),
    };
    connection
        .sender
        .send(lsp_server::Message::Notification(notification))
        .map_err(|e| e.to_string())
}

pub(crate) fn lsp_definition(text: &str, uri: &str, word: &str) -> JsonValue {
    if word.is_empty() {
        return JsonValue::Null;
    }
    let patterns = [
        format!("fn {word}"),
        format!("class {word}"),
        format!("let {word}"),
        format!("const {word}"),
    ];
    for (line_no, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        if patterns.iter().any(|p| trimmed.starts_with(p)) {
            if let Some(col) = line.find(word) {
                return serde_json::json!({
                    "uri": uri,
                    "range": {
                        "start":{"line":line_no,"character":col},
                        "end":{"line":line_no,"character":col+word.chars().count()}
                    }
                });
            }
        }
    }
    JsonValue::Null
}

pub(crate) fn lsp_references(text: &str, uri: &str, word: &str) -> JsonValue {
    if word.is_empty() {
        return JsonValue::Array(vec![]);
    }
    let mut refs = vec![];
    for (line_no, line) in text.lines().enumerate() {
        let chars: Vec<char> = line.chars().collect();
        let target: Vec<char> = word.chars().collect();
        if target.is_empty() || chars.len() < target.len() {
            continue;
        }
        for i in 0..=chars.len() - target.len() {
            if chars[i..i + target.len()] != target[..] {
                continue;
            }
            let left_ok = i == 0 || !(chars[i - 1].is_ascii_alphanumeric() || chars[i - 1] == '_');
            let right = i + target.len();
            let right_ok = right == chars.len()
                || !(chars[right].is_ascii_alphanumeric() || chars[right] == '_');
            if left_ok && right_ok {
                refs.push(serde_json::json!({
                    "uri":uri,
                    "range":{
                        "start":{"line":line_no,"character":i},
                        "end":{"line":line_no,"character":right}
                    }
                }));
            }
        }
    }
    JsonValue::Array(refs)
}

pub(crate) fn lsp_rename(text: &str, uri: &str, word: &str, new_name: &str) -> JsonValue {
    if word.is_empty()
        || new_name.is_empty()
        || !new_name
            .chars()
            .enumerate()
            .all(|(i, c)| c == '_' || c.is_ascii_alphanumeric() && (i > 0 || !c.is_ascii_digit()))
    {
        return JsonValue::Null;
    }
    let refs = lsp_references(text, uri, word);
    let edits = refs
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|location| {
            location.get("range").cloned().map(|range| {
                serde_json::json!({
                    "range": range,
                    "newText": new_name
                })
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({"changes": {uri: edits}})
}

pub(crate) fn lsp_server() -> Result<(), String> {
    let (connection, io_threads) = lsp_server::Connection::stdio();
    let (initialize_id, _) = connection.initialize_start().map_err(|e| e.to_string())?;
    let initialize_result = serde_json::json!({
        "capabilities": {
            "textDocumentSync": 2,
            "completionProvider": {"resolveProvider": false, "triggerCharacters": ["."]},
            "hoverProvider": true,
            "definitionProvider": true,
            "referencesProvider": true,
            "renameProvider": true,
            "documentSymbolProvider": true,
            "documentFormattingProvider": true
        },
        "serverInfo": {"name": "Veyra Language Server", "version": VERSION}
    });
    connection
        .initialize_finish(initialize_id, initialize_result)
        .map_err(|e| e.to_string())?;

    let mut documents: HashMap<String, String> = HashMap::new();
    for message in &connection.receiver {
        match message {
            lsp_server::Message::Request(request) => {
                if connection
                    .handle_shutdown(&request)
                    .map_err(|e| e.to_string())?
                {
                    break;
                }
                let result = match request.method.as_str() {
                    "textDocument/completion" => lsp_completion_items(),
                    "textDocument/hover" => {
                        let uri = request
                            .params
                            .pointer("/textDocument/uri")
                            .and_then(JsonValue::as_str)
                            .unwrap_or("");
                        let line = request
                            .params
                            .pointer("/position/line")
                            .and_then(JsonValue::as_u64)
                            .unwrap_or(0) as usize;
                        let character = request
                            .params
                            .pointer("/position/character")
                            .and_then(JsonValue::as_u64)
                            .unwrap_or(0) as usize;
                        let word = documents
                            .get(uri)
                            .map(|text| lsp_word_at(text, line, character))
                            .unwrap_or_default();
                        if word.is_empty() {
                            JsonValue::Null
                        } else {
                            serde_json::json!({
                                "contents": {
                                    "kind": "markdown",
                                    "value": format!("Veyra symbol: {word}\n\nNative symbol or language construct.")
                                }
                            })
                        }
                    }
                    "textDocument/definition" => {
                        let uri = request
                            .params
                            .pointer("/textDocument/uri")
                            .and_then(JsonValue::as_str)
                            .unwrap_or("");
                        let line = request
                            .params
                            .pointer("/position/line")
                            .and_then(JsonValue::as_u64)
                            .unwrap_or(0) as usize;
                        let character = request
                            .params
                            .pointer("/position/character")
                            .and_then(JsonValue::as_u64)
                            .unwrap_or(0) as usize;
                        let text = documents.get(uri).cloned().unwrap_or_default();
                        let word = lsp_word_at(&text, line, character);
                        lsp_definition(&text, uri, &word)
                    }
                    "textDocument/references" => {
                        let uri = request
                            .params
                            .pointer("/textDocument/uri")
                            .and_then(JsonValue::as_str)
                            .unwrap_or("");
                        let line = request
                            .params
                            .pointer("/position/line")
                            .and_then(JsonValue::as_u64)
                            .unwrap_or(0) as usize;
                        let character = request
                            .params
                            .pointer("/position/character")
                            .and_then(JsonValue::as_u64)
                            .unwrap_or(0) as usize;
                        let text = documents.get(uri).cloned().unwrap_or_default();
                        let word = lsp_word_at(&text, line, character);
                        lsp_references(&text, uri, &word)
                    }
                    "textDocument/rename" => {
                        let uri = request
                            .params
                            .pointer("/textDocument/uri")
                            .and_then(JsonValue::as_str)
                            .unwrap_or("");
                        let line = request
                            .params
                            .pointer("/position/line")
                            .and_then(JsonValue::as_u64)
                            .unwrap_or(0) as usize;
                        let character = request
                            .params
                            .pointer("/position/character")
                            .and_then(JsonValue::as_u64)
                            .unwrap_or(0) as usize;
                        let new_name = request
                            .params
                            .get("newName")
                            .and_then(JsonValue::as_str)
                            .unwrap_or("");
                        let text = documents.get(uri).cloned().unwrap_or_default();
                        let word = lsp_word_at(&text, line, character);
                        lsp_rename(&text, uri, &word, new_name)
                    }
                    "textDocument/documentSymbol" => {
                        let uri = request
                            .params
                            .pointer("/textDocument/uri")
                            .and_then(JsonValue::as_str)
                            .unwrap_or("");
                        let text = documents.get(uri).cloned().unwrap_or_default();
                        let filtered = text
                            .lines()
                            .map(|line| {
                                if line.trim_start().starts_with("use ") {
                                    ""
                                } else {
                                    line
                                }
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        let mut symbols = vec![];
                        if let Ok(tokens) = lex(&filtered) {
                            if let Ok(ast) = Parser::new(tokens).program() {
                                for stmt in ast {
                                    let (name, kind) = match stmt {
                                        Stmt::Fn(name, _, _, _) | Stmt::AsyncFn(name, _, _, _) => {
                                            (name, 12)
                                        }
                                        Stmt::DecoratedFn(_, name, _, _, _) => (name, 12),
                                        Stmt::Class(name, _, _) => (name, 5),
                                        Stmt::Let(name, _, _, _) => (name, 13),
                                        _ => continue,
                                    };
                                    symbols.push(serde_json::json!({
                                        "name": name,
                                        "kind": kind,
                                        "range": {
                                            "start":{"line":0,"character":0},
                                            "end":{"line":0,"character":1}
                                        },
                                        "selectionRange": {
                                            "start":{"line":0,"character":0},
                                            "end":{"line":0,"character":1}
                                        }
                                    }));
                                }
                            }
                        }
                        JsonValue::Array(symbols)
                    }
                    "textDocument/formatting" => {
                        let uri = request
                            .params
                            .pointer("/textDocument/uri")
                            .and_then(JsonValue::as_str)
                            .unwrap_or("");
                        let text = documents.get(uri).cloned().unwrap_or_default();
                        let formatted = fmt_source(&text);
                        let lines = text.lines().collect::<Vec<_>>();
                        let end_line = lines.len().saturating_sub(1);
                        let end_char = lines.last().map(|x| x.chars().count()).unwrap_or(0);
                        serde_json::json!([{
                            "range": {
                                "start":{"line":0,"character":0},
                                "end":{"line":end_line,"character":end_char}
                            },
                            "newText": formatted
                        }])
                    }
                    _ => JsonValue::Null,
                };
                let response = lsp_server::Response::new_ok(request.id.clone(), result);
                connection
                    .sender
                    .send(lsp_server::Message::Response(response))
                    .map_err(|e| e.to_string())?;
            }
            lsp_server::Message::Notification(notification) => match notification.method.as_str() {
                "textDocument/didOpen" => {
                    let uri = notification
                        .params
                        .pointer("/textDocument/uri")
                        .and_then(JsonValue::as_str)
                        .unwrap_or("")
                        .to_string();
                    let text = notification
                        .params
                        .pointer("/textDocument/text")
                        .and_then(JsonValue::as_str)
                        .unwrap_or("")
                        .to_string();
                    documents.insert(uri.clone(), text.clone());
                    lsp_publish_diagnostics(&connection, &uri, &text)?;
                }
                "textDocument/didChange" => {
                    let uri = notification
                        .params
                        .pointer("/textDocument/uri")
                        .and_then(JsonValue::as_str)
                        .unwrap_or("")
                        .to_string();
                    if let Some(text) = notification
                        .params
                        .pointer("/contentChanges/0/text")
                        .and_then(JsonValue::as_str)
                    {
                        documents.insert(uri.clone(), text.to_string());
                        lsp_publish_diagnostics(&connection, &uri, text)?;
                    }
                }
                "textDocument/didClose" => {
                    let uri = notification
                        .params
                        .pointer("/textDocument/uri")
                        .and_then(JsonValue::as_str)
                        .unwrap_or("");
                    documents.remove(uri);
                    lsp_publish_diagnostics(&connection, uri, "")?;
                }
                _ => {}
            },
            lsp_server::Message::Response(_) => {}
        }
    }
    io_threads.join().map_err(|e| e.to_string())?;
    Ok(())
}
