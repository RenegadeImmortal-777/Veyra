// Runtime values and the JSON, SQL, crypto and linear-algebra helpers
// that operate on them.
//
// Extracted from `main.rs` without behavior change; see docs/ENGINE-SPLIT.md.

use serde_json::Value as JsonValue;
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Val {
    Num(f64),
    Str(String),
    Bool(bool),
    Null,
    List(Vec<Val>),
    Map(Vec<(Val, Val)>),
    Object(u64),
    Function(String),
    Generator(u64),
}
pub(crate) fn val_to_json(v: &Val) -> JsonValue {
    match v {
        Val::Num(n) => JsonValue::from(*n),
        Val::Str(s) => JsonValue::from(s.clone()),
        Val::Bool(b) => JsonValue::from(*b),
        Val::Null => JsonValue::Null,
        Val::List(xs) => JsonValue::Array(xs.iter().map(val_to_json).collect()),
        Val::Object(id) => JsonValue::String(format!("<object:{id}>")),
        Val::Function(name) => JsonValue::String(format!("<function:{name}>")),
        Val::Generator(id) => JsonValue::String(format!("<generator:{id}>")),
        Val::Map(pairs) => {
            let mut obj = serde_json::Map::new();
            for (k, v) in pairs {
                obj.insert(k.show(), val_to_json(v));
            }
            JsonValue::Object(obj)
        }
    }
}

#[cfg(feature = "native-services")]
pub(crate) fn val_to_sql_value(v: &Val) -> rusqlite::types::Value {
    match v {
        Val::Null => rusqlite::types::Value::Null,
        Val::Bool(b) => rusqlite::types::Value::Integer(if *b { 1 } else { 0 }),
        Val::Num(n) if n.fract() == 0.0 => rusqlite::types::Value::Integer(*n as i64),
        Val::Num(n) => rusqlite::types::Value::Real(*n),
        Val::Str(s) => rusqlite::types::Value::Text(s.clone()),
        other => rusqlite::types::Value::Text(other.show()),
    }
}

#[cfg(feature = "native-services")]
pub(crate) fn json_stringify_value(v: &Val) -> Result<String, String> {
    serde_json::to_string(&val_to_json(v)).map_err(|e| e.to_string())
}

pub(crate) fn json_to_val(v: &JsonValue) -> Val {
    match v {
        JsonValue::Null => Val::Null,
        JsonValue::Bool(b) => Val::Bool(*b),
        JsonValue::Number(n) => Val::Num(n.as_f64().unwrap_or(0.0)),
        JsonValue::String(s) => Val::Str(s.clone()),
        JsonValue::Array(xs) => Val::List(xs.iter().map(json_to_val).collect()),
        JsonValue::Object(obj) => Val::Map(
            obj.iter()
                .map(|(k, v)| (Val::Str(k.clone()), json_to_val(v)))
                .collect(),
        ),
    }
}

pub(crate) fn as_num_list(v: &Val) -> Result<Vec<f64>, String> {
    match v {
        Val::List(xs) => xs
            .iter()
            .map(|x| match x {
                Val::Num(n) => Ok(*n),
                _ => Err("expected numeric list".into()),
            })
            .collect(),
        _ => Err("expected List".into()),
    }
}

pub(crate) fn as_matrix(v: &Val) -> Result<Vec<Vec<f64>>, String> {
    match v {
        Val::List(rows) => rows.iter().map(as_num_list).collect(),
        _ => Err("expected matrix as List<List<Number>>".into()),
    }
}

pub(crate) fn hmac_sha256_bytes(key: &[u8], message: &[u8]) -> Vec<u8> {
    let block_size = 64usize;
    let mut key_block = vec![0u8; block_size];
    if key.len() > block_size {
        let digest = Sha256::digest(key);
        key_block[..digest.len()].copy_from_slice(&digest);
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut ipad = vec![0x36u8; block_size];
    let mut opad = vec![0x5cu8; block_size];
    for i in 0..block_size {
        ipad[i] ^= key_block[i];
        opad[i] ^= key_block[i];
    }
    let mut inner = Sha256::new();
    inner.update(&ipad);
    inner.update(message);
    let inner_digest = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(&opad);
    outer.update(inner_digest);
    outer.finalize().to_vec()
}

pub(crate) fn hmac_sha256_hex(key: &str, message: &str) -> String {
    hmac_sha256_bytes(key.as_bytes(), message.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub(crate) fn pbkdf2_sha256_hex(
    password: &str,
    salt: &str,
    iterations: u32,
    bytes: usize,
) -> String {
    let mut out = vec![];
    let mut block_index = 1u32;
    while out.len() < bytes {
        let mut seed = salt.as_bytes().to_vec();
        seed.extend_from_slice(&block_index.to_be_bytes());
        let mut u = hmac_sha256_bytes(password.as_bytes(), &seed);
        let mut t = u.clone();
        for _ in 1..iterations.max(1) {
            u = hmac_sha256_bytes(password.as_bytes(), &u);
            for (x, y) in t.iter_mut().zip(u.iter()) {
                *x ^= *y;
            }
        }
        out.extend_from_slice(&t);
        block_index += 1;
    }
    out.truncate(bytes);
    out.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn constant_time_strings_equal(a: &str, b: &str) -> bool {
    let aa = a.as_bytes();
    let bb = b.as_bytes();
    let mut diff = aa.len() ^ bb.len();
    let max_len = aa.len().max(bb.len());
    for i in 0..max_len {
        let x = aa.get(i).copied().unwrap_or(0);
        let y = bb.get(i).copied().unwrap_or(0);
        diff |= (x ^ y) as usize;
    }
    diff == 0
}

pub(crate) fn nums_to_val(xs: Vec<f64>) -> Val {
    Val::List(xs.into_iter().map(Val::Num).collect())
}

pub(crate) fn matrix_to_val(xs: Vec<Vec<f64>>) -> Val {
    Val::List(xs.into_iter().map(nums_to_val).collect())
}

pub(crate) fn distance_sq(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b.iter()).map(|(x, y)| (x - y) * (x - y)).sum()
}

pub(crate) fn square_matrix(m: &[Vec<f64>]) -> Result<usize, String> {
    let n = m.len();
    if n == 0 || m.iter().any(|r| r.len() != n) {
        return Err("expected non-empty square matrix".into());
    }
    Ok(n)
}

pub(crate) fn matrix_determinant(mut m: Vec<Vec<f64>>) -> Result<f64, String> {
    let n = square_matrix(&m)?;
    let mut det = 1.0;
    let mut sign = 1.0;
    for i in 0..n {
        let mut pivot = i;
        for r in (i + 1)..n {
            if m[r][i].abs() > m[pivot][i].abs() {
                pivot = r;
            }
        }
        if m[pivot][i].abs() < 1e-12 {
            return Ok(0.0);
        }
        if pivot != i {
            m.swap(i, pivot);
            sign *= -1.0;
        }
        let pv = m[i][i];
        det *= pv;
        for r in (i + 1)..n {
            let factor = m[r][i] / pv;
            for c in (i + 1)..n {
                m[r][c] -= factor * m[i][c];
            }
        }
    }
    Ok(det * sign)
}

pub(crate) fn matrix_inverse(mut a: Vec<Vec<f64>>) -> Result<Vec<Vec<f64>>, String> {
    let n = square_matrix(&a)?;
    let mut inv = vec![vec![0.0; n]; n];
    for i in 0..n {
        inv[i][i] = 1.0;
    }
    for i in 0..n {
        let mut pivot = i;
        for r in (i + 1)..n {
            if a[r][i].abs() > a[pivot][i].abs() {
                pivot = r;
            }
        }
        if a[pivot][i].abs() < 1e-12 {
            return Err("matrix is singular".into());
        }
        if pivot != i {
            a.swap(i, pivot);
            inv.swap(i, pivot);
        }
        let pv = a[i][i];
        for c in 0..n {
            a[i][c] /= pv;
            inv[i][c] /= pv;
        }
        for r in 0..n {
            if r == i {
                continue;
            }
            let factor = a[r][i];
            for c in 0..n {
                a[r][c] -= factor * a[i][c];
                inv[r][c] -= factor * inv[i][c];
            }
        }
    }
    Ok(inv)
}

pub(crate) fn solve_linear_system(
    mut a: Vec<Vec<f64>>,
    mut b: Vec<f64>,
) -> Result<Vec<f64>, String> {
    let n = square_matrix(&a)?;
    if b.len() != n {
        return Err("linear solve dimension mismatch".into());
    }
    for i in 0..n {
        let mut pivot = i;
        for r in (i + 1)..n {
            if a[r][i].abs() > a[pivot][i].abs() {
                pivot = r;
            }
        }
        if a[pivot][i].abs() < 1e-12 {
            return Err("matrix is singular".into());
        }
        if pivot != i {
            a.swap(i, pivot);
            b.swap(i, pivot);
        }
        let pv = a[i][i];
        for r in (i + 1)..n {
            let factor = a[r][i] / pv;
            for c in i..n {
                a[r][c] -= factor * a[i][c];
            }
            b[r] -= factor * b[i];
        }
    }
    let mut x = vec![0.0; n];
    for i in (0..n).rev() {
        let rhs = b[i] - ((i + 1)..n).map(|j| a[i][j] * x[j]).sum::<f64>();
        x[i] = rhs / a[i][i];
    }
    Ok(x)
}

pub(crate) fn map_lookup<'a>(pairs: &'a [(Val, Val)], key: &Val) -> Option<&'a Val> {
    pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

pub(crate) fn svg_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

impl Val {
    pub(crate) fn truthy(&self) -> bool {
        match self {
            Val::Bool(b) => *b,
            Val::Null => false,
            Val::Num(n) => *n != 0.0,
            Val::Str(s) => !s.is_empty(),
            Val::List(v) => !v.is_empty(),
            Val::Map(v) => !v.is_empty(),
            Val::Object(_) | Val::Function(_) | Val::Generator(_) => true,
        }
    }
    pub(crate) fn show(&self) -> String {
        match self {
            Val::Num(n) => {
                if n.fract() == 0.0 {
                    format!("{}", *n as i64)
                } else {
                    format!("{n}")
                }
            }
            Val::Str(s) => s.clone(),
            Val::Bool(b) => b.to_string(),
            Val::Null => "null".into(),
            Val::List(v) => format!(
                "[{}]",
                v.iter().map(Val::show).collect::<Vec<_>>().join(", ")
            ),
            Val::Map(v) => format!(
                "{{{}}}",
                v.iter()
                    .map(|(k, x)| format!("{}: {}", k.show(), x.show()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Val::Object(id) => format!("<object:{id}>"),
            Val::Function(name) => format!("<function:{name}>"),
            Val::Generator(id) => format!("<generator:{id}>"),
        }
    }
}
