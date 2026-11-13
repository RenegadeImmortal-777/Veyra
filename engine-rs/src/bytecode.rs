// Bytecode operation set and the function, class and object definitions the
// compiler emits and the VM executes.
//
// Extracted from `main.rs` without behavior change; see docs/ENGINE-SPLIT.md.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::{Value as JsonValue, json};
use sha2::{Digest, Sha256};

use crate::value::Val;

#[derive(Clone, Debug)]
pub(crate) enum Op {
    Const(Val),
    Load(String),
    Define(String, bool, Option<String>),
    Store(String),
    LoadLocal(usize),
    DefineLocal(usize, bool, Option<String>),
    StoreLocal(usize),
    Pop,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Neg,
    Not,
    Index,
    SetIndex,
    GetMember(String),
    SetMember(String),
    Call(String, usize),
    CallMethod(String, usize),
    FunctionRef(String),
    Def(
        String,
        Vec<(String, Option<String>)>,
        Option<String>,
        bool,
        Vec<Op>,
    ),
    DefClass(
        String,
        Option<String>,
        Vec<(
            String,
            Vec<(String, Option<String>)>,
            Option<String>,
            bool,
            Vec<Op>,
        )>,
    ),
    Return,
    Yield,
    PushHandler(usize, String),
    PopHandler,
    Throw,
    JumpIfFalse(usize),
    Jump(usize),
    Halt,
}

#[derive(Clone, Debug)]
pub(crate) struct FunctionDef {
    pub(crate) params: Arc<Vec<(String, Option<String>)>>,
    pub(crate) return_type: Option<String>,
    pub(crate) is_async: bool,
    pub(crate) is_generator: bool,
    pub(crate) code: Arc<Vec<Op>>,
    pub(crate) captures: Arc<HashMap<String, Val>>,
    pub(crate) capture_types: Arc<HashMap<String, String>>,
    pub(crate) upvalues: Arc<HashMap<String, Arc<Mutex<Val>>>>,
    pub(crate) local_count: usize,
    pub(crate) param_named: Arc<Vec<bool>>,
    pub(crate) debug_name: String,
    pub(crate) debug_scope: usize,
}

pub(crate) fn compute_local_count(code: &[Op], param_len: usize) -> usize {
    let mut max_plus_one = param_len;
    for op in code {
        if let Op::LoadLocal(s) | Op::DefineLocal(s, _, _) | Op::StoreLocal(s) = op {
            if *s + 1 > max_plus_one {
                max_plus_one = *s + 1;
            }
        }
    }
    max_plus_one
}

pub(crate) fn code_has_yield(code: &[Op]) -> bool {
    code.iter().any(|op| matches!(op, Op::Yield))
}

pub(crate) fn collect_referenced_names(code: &[Op], out: &mut std::collections::HashSet<String>) {
    for op in code {
        match op {
            Op::Load(n) | Op::Store(n) | Op::Call(n, _) | Op::FunctionRef(n) => {
                out.insert(n.clone());
            }
            Op::Def(_, _, _, _, body) => {
                collect_referenced_names(body, out);
            }
            Op::DefClass(_, _, methods) => {
                for (_, _, _, _, body) in methods {
                    collect_referenced_names(body, out);
                }
            }
            _ => {}
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ClassDef {
    pub(crate) parent: Option<String>,
    pub(crate) methods: HashMap<String, FunctionDef>,
}

#[derive(Clone, Debug)]
pub(crate) struct ObjectData {
    pub(crate) class_name: String,
    pub(crate) fields: HashMap<String, Val>,
}

fn encode_val(v: &Val) -> JsonValue {
    match v {
        Val::Null => json!({"t": "null"}),
        Val::Bool(b) => json!({"t": "bool", "v": b}),
        Val::Num(n) => {
            json!({"t": "num", "v": if n.fract() == 0.0 { json!(*n as i64) } else { json!(*n) }})
        }
        Val::Str(s) => json!({"t": "str", "v": s}),
        Val::List(xs) => json!({"t": "list", "v": xs.iter().map(encode_val).collect::<Vec<_>>()}),
        Val::Map(pairs) => json!({
            "t": "map",
            "v": pairs.iter().map(|(k, val)| json!([encode_val(k), encode_val(val)])).collect::<Vec<_>>()
        }),
        Val::Function(name) => json!({"t": "fn", "v": name}),
        Val::Object(id) => json!({"t": "obj", "v": id}),
        Val::Generator(id) => json!({"t": "gen", "v": id}),
    }
}

fn decode_val(j: &JsonValue) -> Result<Val, String> {
    let t = j
        .get("t")
        .and_then(JsonValue::as_str)
        .ok_or("vybc: missing value type")?;
    match t {
        "null" => Ok(Val::Null),
        "bool" => Ok(Val::Bool(
            j.get("v")
                .and_then(JsonValue::as_bool)
                .ok_or("vybc: invalid bool")?,
        )),
        "num" => Ok(Val::Num(
            j.get("v")
                .and_then(JsonValue::as_f64)
                .ok_or("vybc: invalid num")?,
        )),
        "str" => Ok(Val::Str(
            j.get("v")
                .and_then(JsonValue::as_str)
                .ok_or("vybc: invalid str")?
                .to_string(),
        )),
        "list" => {
            let arr = j
                .get("v")
                .and_then(JsonValue::as_array)
                .ok_or("vybc: invalid list")?;
            arr.iter()
                .map(decode_val)
                .collect::<Result<Vec<_>, _>>()
                .map(Val::List)
        }
        "map" => {
            let arr = j
                .get("v")
                .and_then(JsonValue::as_array)
                .ok_or("vybc: invalid map")?;
            let mut pairs = Vec::with_capacity(arr.len());
            for item in arr {
                let pair = item.as_array().ok_or("vybc: invalid map entry")?;
                if pair.len() != 2 {
                    return Err("vybc: map entry must have 2 elements".into());
                }
                pairs.push((decode_val(&pair[0])?, decode_val(&pair[1])?));
            }
            Ok(Val::Map(pairs))
        }
        "fn" => Ok(Val::Function(
            j.get("v")
                .and_then(JsonValue::as_str)
                .ok_or("vybc: invalid fn")?
                .to_string(),
        )),
        _ => Err(format!("vybc: unsupported value tag '{t}'")),
    }
}

fn encode_ops(code: &[Op]) -> JsonValue {
    JsonValue::Array(
        code.iter()
            .map(|op| match op {
                Op::Const(v) => json!({"op": "Const", "val": encode_val(v)}),
                Op::Load(n) => json!({"op": "Load", "name": n}),
                Op::Define(n, c, ty) => json!({"op": "Define", "name": n, "const": c, "type": ty}),
                Op::Store(n) => json!({"op": "Store", "name": n}),
                Op::LoadLocal(s) => json!({"op": "LoadLocal", "slot": s}),
                Op::DefineLocal(s, c, ty) => {
                    json!({"op": "DefineLocal", "slot": s, "const": c, "type": ty})
                }
                Op::StoreLocal(s) => json!({"op": "StoreLocal", "slot": s}),
                Op::Pop => json!({"op": "Pop"}),
                Op::Add => json!({"op": "Add"}),
                Op::Sub => json!({"op": "Sub"}),
                Op::Mul => json!({"op": "Mul"}),
                Op::Div => json!({"op": "Div"}),
                Op::Mod => json!({"op": "Mod"}),
                Op::Pow => json!({"op": "Pow"}),
                Op::Eq => json!({"op": "Eq"}),
                Op::Ne => json!({"op": "Ne"}),
                Op::Lt => json!({"op": "Lt"}),
                Op::Le => json!({"op": "Le"}),
                Op::Gt => json!({"op": "Gt"}),
                Op::Ge => json!({"op": "Ge"}),
                Op::Neg => json!({"op": "Neg"}),
                Op::Not => json!({"op": "Not"}),
                Op::Index => json!({"op": "Index"}),
                Op::SetIndex => json!({"op": "SetIndex"}),
                Op::GetMember(n) => json!({"op": "GetMember", "name": n}),
                Op::SetMember(n) => json!({"op": "SetMember", "name": n}),
                Op::Call(n, k) => json!({"op": "Call", "name": n, "argc": k}),
                Op::CallMethod(n, k) => json!({"op": "CallMethod", "name": n, "argc": k}),
                Op::FunctionRef(n) => json!({"op": "FunctionRef", "name": n}),
                Op::Def(name, params, ret, is_async, body) => json!({
                    "op": "Def",
                    "name": name,
                    "params": params.iter().map(|(p, t)| json!([p, t])).collect::<Vec<_>>(),
                    "ret": ret,
                    "async": is_async,
                    "body": encode_ops(body)
                }),
                Op::DefClass(name, parent, methods) => json!({
                    "op": "DefClass",
                    "name": name,
                    "parent": parent,
                    "methods": methods.iter().map(|(m, params, ret, is_async, body)| json!({
                        "name": m,
                        "params": params.iter().map(|(p, t)| json!([p, t])).collect::<Vec<_>>(),
                        "ret": ret,
                        "async": is_async,
                        "body": encode_ops(body)
                    })).collect::<Vec<_>>()
                }),
                Op::Return => json!({"op": "Return"}),
                Op::Yield => json!({"op": "Yield"}),
                Op::PushHandler(target, name) => {
                    json!({"op": "PushHandler", "target": target, "name": name})
                }
                Op::PopHandler => json!({"op": "PopHandler"}),
                Op::Throw => json!({"op": "Throw"}),
                Op::JumpIfFalse(t) => json!({"op": "JumpIfFalse", "target": t}),
                Op::Jump(t) => json!({"op": "Jump", "target": t}),
                Op::Halt => json!({"op": "Halt"}),
            })
            .collect(),
    )
}

fn decode_params(j: &JsonValue) -> Result<Vec<(String, Option<String>)>, String> {
    let arr = j.as_array().ok_or("vybc: invalid params")?;
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        if let Some(pair) = item.as_array() {
            let name = pair
                .first()
                .and_then(JsonValue::as_str)
                .ok_or("vybc: invalid param name")?
                .to_string();
            let ty = pair
                .get(1)
                .and_then(JsonValue::as_str)
                .map(ToString::to_string);
            out.push((name, ty));
        } else if let Some(obj) = item.as_object() {
            let name = obj
                .get("name")
                .and_then(JsonValue::as_str)
                .ok_or("vybc: invalid param name")?
                .to_string();
            let ty = obj
                .get("type")
                .and_then(JsonValue::as_str)
                .map(ToString::to_string);
            out.push((name, ty));
        } else {
            return Err("vybc: invalid param pair".into());
        }
    }
    Ok(out)
}

fn json_as_usize(v: &JsonValue) -> Option<usize> {
    v.as_u64()
        .map(|n| n as usize)
        .or_else(|| v.as_f64().map(|n| n as usize))
}

fn decode_ops(j: &JsonValue) -> Result<Vec<Op>, String> {
    let arr = j.as_array().ok_or("vybc: ops must be an array")?;
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let tag = item
            .get("op")
            .and_then(JsonValue::as_str)
            .ok_or("vybc: missing op tag")?;
        let op = match tag {
            "Const" => Op::Const(decode_val(
                item.get("val").ok_or("vybc: Const missing val")?,
            )?),
            "Load" => Op::Load(
                item.get("name")
                    .and_then(JsonValue::as_str)
                    .ok_or("vybc: Load missing name")?
                    .to_string(),
            ),
            "Define" => Op::Define(
                item.get("name")
                    .and_then(JsonValue::as_str)
                    .ok_or("vybc: Define missing name")?
                    .to_string(),
                item.get("const")
                    .and_then(JsonValue::as_bool)
                    .unwrap_or(false),
                item.get("type")
                    .and_then(JsonValue::as_str)
                    .map(ToString::to_string),
            ),
            "Store" => Op::Store(
                item.get("name")
                    .and_then(JsonValue::as_str)
                    .ok_or("vybc: Store missing name")?
                    .to_string(),
            ),
            "LoadLocal" => Op::LoadLocal(
                item.get("slot")
                    .and_then(json_as_usize)
                    .ok_or("vybc: LoadLocal missing slot")?,
            ),
            "DefineLocal" => Op::DefineLocal(
                item.get("slot")
                    .and_then(json_as_usize)
                    .ok_or("vybc: DefineLocal missing slot")?,
                item.get("const")
                    .and_then(JsonValue::as_bool)
                    .unwrap_or(false),
                item.get("type")
                    .and_then(JsonValue::as_str)
                    .map(ToString::to_string),
            ),
            "StoreLocal" => Op::StoreLocal(
                item.get("slot")
                    .and_then(json_as_usize)
                    .ok_or("vybc: StoreLocal missing slot")?,
            ),
            "Pop" => Op::Pop,
            "Add" => Op::Add,
            "Sub" => Op::Sub,
            "Mul" => Op::Mul,
            "Div" => Op::Div,
            "Mod" => Op::Mod,
            "Pow" => Op::Pow,
            "Eq" => Op::Eq,
            "Ne" => Op::Ne,
            "Lt" => Op::Lt,
            "Le" => Op::Le,
            "Gt" => Op::Gt,
            "Ge" => Op::Ge,
            "Neg" => Op::Neg,
            "Not" => Op::Not,
            "Index" => Op::Index,
            "SetIndex" => Op::SetIndex,
            "GetMember" => Op::GetMember(
                item.get("name")
                    .and_then(JsonValue::as_str)
                    .ok_or("vybc: GetMember missing name")?
                    .to_string(),
            ),
            "SetMember" => Op::SetMember(
                item.get("name")
                    .and_then(JsonValue::as_str)
                    .ok_or("vybc: SetMember missing name")?
                    .to_string(),
            ),
            "Call" => Op::Call(
                item.get("name")
                    .and_then(JsonValue::as_str)
                    .ok_or("vybc: Call missing name")?
                    .to_string(),
                item.get("argc")
                    .and_then(json_as_usize)
                    .ok_or("vybc: Call missing argc")?,
            ),
            "CallMethod" => Op::CallMethod(
                item.get("name")
                    .and_then(JsonValue::as_str)
                    .ok_or("vybc: CallMethod missing name")?
                    .to_string(),
                item.get("argc")
                    .and_then(json_as_usize)
                    .ok_or("vybc: CallMethod missing argc")?,
            ),
            "FunctionRef" => Op::FunctionRef(
                item.get("name")
                    .and_then(JsonValue::as_str)
                    .ok_or("vybc: FunctionRef missing name")?
                    .to_string(),
            ),
            "Def" => Op::Def(
                item.get("name")
                    .and_then(JsonValue::as_str)
                    .ok_or("vybc: Def missing name")?
                    .to_string(),
                decode_params(item.get("params").ok_or("vybc: Def missing params")?)?,
                item.get("ret")
                    .and_then(JsonValue::as_str)
                    .map(ToString::to_string),
                item.get("async")
                    .and_then(JsonValue::as_bool)
                    .unwrap_or(false),
                decode_ops(item.get("body").ok_or("vybc: Def missing body")?)?,
            ),
            "DefClass" => {
                let name = item
                    .get("name")
                    .and_then(JsonValue::as_str)
                    .ok_or("vybc: DefClass missing name")?
                    .to_string();
                let parent = item
                    .get("parent")
                    .and_then(JsonValue::as_str)
                    .map(ToString::to_string);
                let methods_arr = item
                    .get("methods")
                    .and_then(JsonValue::as_array)
                    .ok_or("vybc: DefClass missing methods")?;
                let mut methods = Vec::with_capacity(methods_arr.len());
                for m in methods_arr {
                    methods.push((
                        m.get("name")
                            .and_then(JsonValue::as_str)
                            .ok_or("vybc: method missing name")?
                            .to_string(),
                        decode_params(m.get("params").ok_or("vybc: method missing params")?)?,
                        m.get("ret")
                            .and_then(JsonValue::as_str)
                            .map(ToString::to_string),
                        m.get("async").and_then(JsonValue::as_bool).unwrap_or(false),
                        decode_ops(m.get("body").ok_or("vybc: method missing body")?)?,
                    ));
                }
                Op::DefClass(name, parent, methods)
            }
            "Return" => Op::Return,
            "Yield" => Op::Yield,
            "PushHandler" => Op::PushHandler(
                item.get("target")
                    .and_then(json_as_usize)
                    .ok_or("vybc: PushHandler missing target")?,
                item.get("name")
                    .and_then(JsonValue::as_str)
                    .ok_or("vybc: PushHandler missing name")?
                    .to_string(),
            ),
            "PopHandler" => Op::PopHandler,
            "Throw" => Op::Throw,
            "JumpIfFalse" => Op::JumpIfFalse(
                item.get("target")
                    .and_then(json_as_usize)
                    .ok_or("vybc: JumpIfFalse missing target")?,
            ),
            "Jump" => Op::Jump(
                item.get("target")
                    .and_then(json_as_usize)
                    .ok_or("vybc: Jump missing target")?,
            ),
            "Halt" => Op::Halt,
            other => return Err(format!("vybc: unknown opcode '{other}'")),
        };
        out.push(op);
    }
    Ok(out)
}

pub(crate) fn encode_bytecode(code: &[Op]) -> Result<String, String> {
    let ops = encode_ops(code);
    let canonical_ops = serde_json::to_string(&ops).map_err(|e| e.to_string())?;
    let sha = format!("{:x}", Sha256::digest(canonical_ops.as_bytes()));
    let doc = json!({
        "format": "veyra-bc-v1",
        "sha256": sha,
        "ops": ops,
    });
    serde_json::to_string(&doc).map_err(|e| e.to_string())
}

pub(crate) fn decode_bytecode(text: &str) -> Result<Vec<Op>, String> {
    let doc: JsonValue =
        serde_json::from_str(text).map_err(|e| format!("invalid vybc JSON: {e}"))?;
    let fmt = doc
        .get("format")
        .and_then(JsonValue::as_str)
        .ok_or("vybc: missing format header")?;
    if fmt != "veyra-bc-v1" {
        return Err(format!("vybc: unsupported format '{fmt}'"));
    }
    let expected_sha = doc
        .get("sha256")
        .and_then(JsonValue::as_str)
        .ok_or("vybc: missing sha256")?;
    let ops = doc.get("ops").ok_or("vybc: missing ops array")?;
    let canonical_ops = serde_json::to_string(ops).map_err(|e| e.to_string())?;
    let actual_sha = format!("{:x}", Sha256::digest(canonical_ops.as_bytes()));
    if actual_sha != expected_sha {
        return Err(format!(
            "vybc checksum mismatch: expected {expected_sha}, got {actual_sha}"
        ));
    }
    decode_ops(ops)
}
