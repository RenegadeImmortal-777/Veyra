#![allow(
    clippy::collapsible_if,
    clippy::needless_range_loop,
    clippy::type_complexity,
    clippy::let_and_return,
    clippy::only_used_in_recursion,
    unused_imports
)]

use std::{
    collections::{HashMap, HashSet},
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::Ordering,
        mpsc::{self, TryRecvError},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::Engine;
use libloading::Library;
use rand::Rng;
use regex::Regex;
#[cfg(feature = "native-services")]
use rusqlite::Connection;
use serde_json::Value as JsonValue;
use sha2::{Digest, Sha256};

use crate::bytecode::{ClassDef, FunctionDef, ObjectData, Op};
use crate::pipeline::{compile_source, load_source};
use crate::project::project_root;
use crate::runtime::{
    CHANNEL_IDS, FUNCTION_IDS, GENERATOR_IDS, OBJECT_IDS, TASK_IDS, channels,
    export_function_object, namespace_module_ops, register_named_function_object,
    resolve_function_object, tasks,
};
use crate::tensor::{
    AdamState, TensorOp, adam_states, broadcast_index, broadcast_shape, tensor_backward,
    tensor_from_value, tensor_id, tensor_snapshot, tensor_store, tensors,
};
use crate::value::{
    Val, as_matrix, as_num_list, constant_time_strings_equal, distance_sq, hmac_sha256_hex,
    json_to_val, map_lookup, matrix_determinant, matrix_inverse, matrix_to_val, nums_to_val,
    pbkdf2_sha256_hex, solve_linear_system, svg_escape, val_to_json,
};
#[cfg(feature = "native-services")]
use crate::value::{json_stringify_value, val_to_sql_value};
use crate::vm::{Vm, collect_generator, generators, resume_generator};

impl Vm {
    pub(crate) fn call_builtin_science_ml(
        &mut self,
        n: &str,
        a: Vec<Val>,
    ) -> Result<Option<Val>, String> {
        let v = match n {
            "vec_sum" => {
                if a.len() != 1 {
                    return Err("vec_sum expects list".into());
                }
                let xs = as_num_list(&a[0])?;
                Val::Num(xs.iter().sum())
            }
            "vec_mean" => {
                if a.len() != 1 {
                    return Err("vec_mean expects list".into());
                }
                let xs = as_num_list(&a[0])?;
                if xs.is_empty() {
                    return Err("vec_mean requires non-empty list".into());
                }
                Val::Num(xs.iter().sum::<f64>() / xs.len() as f64)
            }
            "vec_variance" => {
                if a.len() != 1 {
                    return Err("vec_variance expects list".into());
                }
                let xs = as_num_list(&a[0])?;
                if xs.is_empty() {
                    return Err("vec_variance requires non-empty list".into());
                }
                let m = xs.iter().sum::<f64>() / xs.len() as f64;
                Val::Num(xs.iter().map(|x| (x - m) * (x - m)).sum::<f64>() / xs.len() as f64)
            }
            "vec_std" => {
                if a.len() != 1 {
                    return Err("vec_std expects list".into());
                }
                let xs = as_num_list(&a[0])?;
                if xs.is_empty() {
                    return Err("vec_std requires non-empty list".into());
                }
                let m = xs.iter().sum::<f64>() / xs.len() as f64;
                let v = xs.iter().map(|x| (x - m) * (x - m)).sum::<f64>() / xs.len() as f64;
                Val::Num(v.sqrt())
            }
            "vec_median" => {
                if a.len() != 1 {
                    return Err("vec_median expects list".into());
                }
                let mut xs = as_num_list(&a[0])?;
                if xs.is_empty() {
                    return Err("vec_median requires non-empty list".into());
                }
                xs.sort_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal));
                let n = xs.len();
                Val::Num(if n % 2 == 1 {
                    xs[n / 2]
                } else {
                    (xs[n / 2 - 1] + xs[n / 2]) / 2.0
                })
            }
            "vec_percentile" => {
                if a.len() != 2 {
                    return Err("vec_percentile expects list,p".into());
                }
                let mut xs = as_num_list(&a[0])?;
                let p = match a[1] {
                    Val::Num(n) => n.clamp(0.0, 100.0),
                    _ => return Err("p must be Number".into()),
                };
                if xs.is_empty() {
                    return Err("vec_percentile requires non-empty list".into());
                }
                xs.sort_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal));
                let pos = (p / 100.0) * (xs.len() - 1) as f64;
                let lo = pos.floor() as usize;
                let hi = pos.ceil() as usize;
                let value = if lo == hi {
                    xs[lo]
                } else {
                    xs[lo] + (xs[hi] - xs[lo]) * (pos - lo as f64)
                };
                Val::Num(value)
            }
            "covariance" => {
                if a.len() != 2 {
                    return Err("covariance expects two lists".into());
                }
                let x = as_num_list(&a[0])?;
                let y = as_num_list(&a[1])?;
                if x.len() != y.len() || x.is_empty() {
                    return Err("covariance length mismatch".into());
                }
                let mx = x.iter().sum::<f64>() / x.len() as f64;
                let my = y.iter().sum::<f64>() / y.len() as f64;
                Val::Num(
                    x.iter()
                        .zip(y.iter())
                        .map(|(a, b)| (a - mx) * (b - my))
                        .sum::<f64>()
                        / x.len() as f64,
                )
            }
            "correlation" => {
                if a.len() != 2 {
                    return Err("correlation expects two lists".into());
                }
                let x = as_num_list(&a[0])?;
                let y = as_num_list(&a[1])?;
                if x.len() != y.len() || x.is_empty() {
                    return Err("correlation length mismatch".into());
                }
                let mx = x.iter().sum::<f64>() / x.len() as f64;
                let my = y.iter().sum::<f64>() / y.len() as f64;
                let cov = x
                    .iter()
                    .zip(y.iter())
                    .map(|(a, b)| (a - mx) * (b - my))
                    .sum::<f64>();
                let sx = x.iter().map(|a| (a - mx) * (a - mx)).sum::<f64>().sqrt();
                let sy = y.iter().map(|b| (b - my) * (b - my)).sum::<f64>().sqrt();
                Val::Num(if sx == 0.0 || sy == 0.0 {
                    0.0
                } else {
                    cov / (sx * sy)
                })
            }
            "vec_dot" => {
                if a.len() != 2 {
                    return Err("vec_dot expects two lists".into());
                }
                let x = as_num_list(&a[0])?;
                let y = as_num_list(&a[1])?;
                if x.len() != y.len() {
                    return Err("vec_dot length mismatch".into());
                }
                Val::Num(x.iter().zip(y.iter()).map(|(a, b)| a * b).sum())
            }
            "vec_add" => {
                if a.len() != 2 {
                    return Err("vec_add expects two lists".into());
                }
                let x = as_num_list(&a[0])?;
                let y = as_num_list(&a[1])?;
                if x.len() != y.len() {
                    return Err("vec_add length mismatch".into());
                }
                nums_to_val(x.iter().zip(y.iter()).map(|(a, b)| a + b).collect())
            }
            "vec_scale" => {
                if a.len() != 2 {
                    return Err("vec_scale expects list,scalar".into());
                }
                let x = as_num_list(&a[0])?;
                let s = match a[1] {
                    Val::Num(n) => n,
                    _ => return Err("scalar must be Number".into()),
                };
                nums_to_val(x.into_iter().map(|v| v * s).collect())
            }
            "transpose" => {
                if a.len() != 1 {
                    return Err("transpose expects matrix".into());
                }
                let m = as_matrix(&a[0])?;
                if m.is_empty() {
                    Val::List(vec![])
                } else {
                    let cols = m[0].len();
                    if m.iter().any(|r| r.len() != cols) {
                        return Err("ragged matrix".into());
                    }
                    let mut out = vec![vec![0.0; m.len()]; cols];
                    for i in 0..m.len() {
                        for j in 0..cols {
                            out[j][i] = m[i][j];
                        }
                    }
                    matrix_to_val(out)
                }
            }
            "matmul" => {
                if a.len() != 2 {
                    return Err("matmul expects two matrices".into());
                }
                let x = as_matrix(&a[0])?;
                let y = as_matrix(&a[1])?;
                if x.is_empty() || y.is_empty() {
                    return Err("matmul requires non-empty matrices".into());
                }
                let n = x[0].len();
                if x.iter().any(|r| r.len() != n)
                    || y.iter().any(|r| r.len() != y[0].len())
                    || y.len() != n
                {
                    return Err("matmul dimension mismatch".into());
                }
                let p = y[0].len();
                let mut out = vec![vec![0.0; p]; x.len()];
                for i in 0..x.len() {
                    for j in 0..p {
                        for k in 0..n {
                            out[i][j] += x[i][k] * y[k][j];
                        }
                    }
                }
                matrix_to_val(out)
            }
            "matrix_det" => {
                if a.len() != 1 {
                    return Err("matrix_det expects matrix".into());
                }
                Val::Num(matrix_determinant(as_matrix(&a[0])?)?)
            }
            "matrix_inverse" => {
                if a.len() != 1 {
                    return Err("matrix_inverse expects matrix".into());
                }
                matrix_to_val(matrix_inverse(as_matrix(&a[0])?)?)
            }
            "linear_solve" => {
                if a.len() != 2 {
                    return Err("linear_solve expects matrix,vector".into());
                }
                nums_to_val(solve_linear_system(as_matrix(&a[0])?, as_num_list(&a[1])?)?)
            }
            "linreg_fit" => {
                if a.len() != 2 {
                    return Err("linreg_fit expects x,y".into());
                }
                let x = as_num_list(&a[0])?;
                let y = as_num_list(&a[1])?;
                if x.len() != y.len() || x.len() < 2 {
                    return Err("linreg_fit requires equal lists length >=2".into());
                }
                let mx = x.iter().sum::<f64>() / x.len() as f64;
                let my = y.iter().sum::<f64>() / y.len() as f64;
                let num = x
                    .iter()
                    .zip(y.iter())
                    .map(|(a, b)| (a - mx) * (b - my))
                    .sum::<f64>();
                let den = x.iter().map(|a| (a - mx) * (a - mx)).sum::<f64>();
                if den == 0.0 {
                    return Err("linreg_fit zero x variance".into());
                }
                let slope = num / den;
                let intercept = my - slope * mx;
                let ss_tot = y.iter().map(|v| (v - my) * (v - my)).sum::<f64>();
                let ss_res = x
                    .iter()
                    .zip(y.iter())
                    .map(|(xx, yy)| {
                        let e = yy - (intercept + slope * xx);
                        e * e
                    })
                    .sum::<f64>();
                let r2 = if ss_tot == 0.0 {
                    1.0
                } else {
                    1.0 - ss_res / ss_tot
                };
                Val::Map(vec![
                    (Val::Str("slope".into()), Val::Num(slope)),
                    (Val::Str("intercept".into()), Val::Num(intercept)),
                    (Val::Str("r2".into()), Val::Num(r2)),
                ])
            }
            "linreg_predict" => {
                if a.len() != 2 {
                    return Err("linreg_predict expects model,x".into());
                }
                let pairs = match &a[0] {
                    Val::Map(p) => p,
                    _ => return Err("model must be Map".into()),
                };
                let getn = |name: &str| -> Result<f64, String> {
                    pairs
                        .iter()
                        .find(|(k, _)| *k == Val::Str(name.into()))
                        .and_then(|(_, v)| if let Val::Num(n) = v { Some(*n) } else { None })
                        .ok_or(format!("model missing {name}"))
                };
                let x = match a[1] {
                    Val::Num(n) => n,
                    _ => return Err("x must be Number".into()),
                };
                Val::Num(getn("intercept")? + getn("slope")? * x)
            }
            "knn_predict" => {
                if a.len() != 4 {
                    return Err("knn_predict expects train_x,train_y,point,k".into());
                }
                let xs = as_matrix(&a[0])?;
                let ys = match &a[1] {
                    Val::List(v) => v.clone(),
                    _ => return Err("train_y must be List".into()),
                };
                let point = as_num_list(&a[2])?;
                let k = match a[3] {
                    Val::Num(n) => n as usize,
                    _ => return Err("k must be Number".into()),
                };
                if xs.len() != ys.len() || xs.is_empty() || k == 0 {
                    return Err("invalid knn data".into());
                }
                let mut pairs: Vec<(f64, Val)> = xs
                    .iter()
                    .zip(ys.iter())
                    .map(|(x, y)| (distance_sq(x, &point), y.clone()))
                    .collect();
                pairs.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
                let mut counts: Vec<(Val, usize)> = vec![];
                for (_, label) in pairs.into_iter().take(k.min(xs.len())) {
                    if let Some((_, c)) = counts.iter_mut().find(|(l, _)| *l == label) {
                        *c += 1
                    } else {
                        counts.push((label, 1))
                    }
                }
                counts
                    .into_iter()
                    .max_by_key(|(_, c)| *c)
                    .map(|(l, _)| l)
                    .unwrap_or(Val::Null)
            }
            "sigmoid" => {
                if a.len() != 1 {
                    return Err("sigmoid expects number".into());
                }
                let x = match a[0] {
                    Val::Num(n) => n,
                    _ => return Err("sigmoid expects Number".into()),
                };
                Val::Num(1.0 / (1.0 + (-x).exp()))
            }
            "tensor" => {
                if a.is_empty() || a.len() > 2 {
                    return Err("tensor expects values[,requires_grad]".into());
                }
                let requires_grad = if a.len() == 2 {
                    match a[1] {
                        Val::Bool(b) => b,
                        _ => return Err("requires_grad must be Bool".into()),
                    }
                } else {
                    false
                };
                tensor_from_value(&a[0], requires_grad)?
            }
            "tensor_values" => {
                if a.len() != 1 {
                    return Err("tensor_values expects tensor".into());
                }
                let id = tensor_id(&a[0])?;
                nums_to_val(tensor_snapshot(id)?.data)
            }
            "tensor_shape" => {
                if a.len() != 1 {
                    return Err("tensor_shape expects tensor".into());
                }
                let id = tensor_id(&a[0])?;
                Val::List(
                    tensor_snapshot(id)?
                        .shape
                        .into_iter()
                        .map(|n| Val::Num(n as f64))
                        .collect(),
                )
            }
            "tensor_grad" => {
                if a.len() != 1 {
                    return Err("tensor_grad expects tensor".into());
                }
                let id = tensor_id(&a[0])?;
                nums_to_val(tensor_snapshot(id)?.grad)
            }
            "tensor_add" | "tensor_sub" | "tensor_mul" | "tensor_div" => {
                if a.len() != 2 {
                    return Err(format!("{n} expects two tensors"));
                }
                let left_id = tensor_id(&a[0])?;
                let right_id = tensor_id(&a[1])?;
                let left = tensor_snapshot(left_id)?;
                let right = tensor_snapshot(right_id)?;
                let out_shape = broadcast_shape(&left.shape, &right.shape)?;
                if n == "tensor_div" && right.data.contains(&0.0) {
                    return Err("tensor_div division by zero".into());
                }
                let out_len = if out_shape.is_empty() {
                    1
                } else {
                    out_shape.iter().product()
                };
                let mut data = Vec::with_capacity(out_len);
                for i in 0..out_len {
                    let ia = broadcast_index(i, &out_shape, &left.shape);
                    let ib = broadcast_index(i, &out_shape, &right.shape);
                    let x = left.data[ia];
                    let y = right.data[ib];
                    let v = match n {
                        "tensor_add" => x + y,
                        "tensor_sub" => x - y,
                        "tensor_mul" => x * y,
                        _ => x / y,
                    };
                    data.push(v);
                }
                let requires_grad = left.requires_grad || right.requires_grad;
                let op = match n {
                    "tensor_add" => TensorOp::Add(left_id, right_id),
                    "tensor_sub" => TensorOp::Sub(left_id, right_id),
                    "tensor_mul" => TensorOp::Mul(left_id, right_id),
                    _ => TensorOp::Div(left_id, right_id),
                };
                tensor_store(data, out_shape, requires_grad, op)?
            }
            "tensor_neg" => {
                if a.len() != 1 {
                    return Err("tensor_neg expects tensor".into());
                }
                let id = tensor_id(&a[0])?;
                let t = tensor_snapshot(id)?;
                let data = t.data.iter().map(|x| -*x).collect();
                tensor_store(data, t.shape, t.requires_grad, TensorOp::Neg(id))?
            }
            "tensor_pow" => {
                if a.len() != 2 {
                    return Err("tensor_pow expects tensor,exponent".into());
                }
                let id = tensor_id(&a[0])?;
                let exponent = match a[1] {
                    Val::Num(n) => n,
                    _ => return Err("tensor_pow exponent must be Number".into()),
                };
                let t = tensor_snapshot(id)?;
                let data = t.data.iter().map(|x| x.powf(exponent)).collect();
                tensor_store(data, t.shape, t.requires_grad, TensorOp::Pow(id, exponent))?
            }
            "tensor_softmax" => {
                if a.len() != 1 {
                    return Err("tensor_softmax expects tensor".into());
                }
                let id = tensor_id(&a[0])?;
                let t = tensor_snapshot(id)?;
                let (rows, cols) = match t.shape.as_slice() {
                    [n] => (1usize, *n),
                    [r, c] => (*r, *c),
                    _ => return Err("tensor_softmax expects 1D or 2D tensor".into()),
                };
                let mut out = vec![0.0; t.data.len()];
                for row in 0..rows {
                    let start = row * cols;
                    let end = start + cols;
                    let max = t.data[start..end]
                        .iter()
                        .copied()
                        .fold(f64::NEG_INFINITY, f64::max);
                    let mut denom = 0.0;
                    for c in 0..cols {
                        let e = (t.data[start + c] - max).exp();
                        out[start + c] = e;
                        denom += e;
                    }
                    for c in 0..cols {
                        out[start + c] /= denom;
                    }
                }
                tensor_store(
                    out,
                    t.shape,
                    t.requires_grad,
                    TensorOp::SoftmaxRows(id, rows, cols),
                )?
            }
            "tensor_detach" => {
                if a.len() != 1 {
                    return Err("tensor_detach expects tensor".into());
                }
                let id = tensor_id(&a[0])?;
                let t = tensor_snapshot(id)?;
                tensor_store(t.data, t.shape, false, TensorOp::Leaf)?
            }
            "tensor_numel" => {
                if a.len() != 1 {
                    return Err("tensor_numel expects tensor".into());
                }
                let id = tensor_id(&a[0])?;
                Val::Num(tensor_snapshot(id)?.data.len() as f64)
            }
            "tensor_relu" => {
                if a.len() != 1 {
                    return Err("tensor_relu expects tensor".into());
                }
                let id = tensor_id(&a[0])?;
                let t = tensor_snapshot(id)?;
                let data = t.data.iter().map(|x| x.max(0.0)).collect();
                tensor_store(data, t.shape, t.requires_grad, TensorOp::Relu(id))?
            }
            "tensor_sigmoid" | "tensor_tanh" | "tensor_exp" | "tensor_log" => {
                if a.len() != 1 {
                    return Err(format!("{n} expects tensor"));
                }
                let id = tensor_id(&a[0])?;
                let t = tensor_snapshot(id)?;
                let data: Vec<f64> = match n {
                    "tensor_sigmoid" => t.data.iter().map(|x| 1.0 / (1.0 + (-x).exp())).collect(),
                    "tensor_tanh" => t.data.iter().map(|x| x.tanh()).collect(),
                    "tensor_exp" => t.data.iter().map(|x| x.exp()).collect(),
                    _ => {
                        if t.data.iter().any(|x| *x <= 0.0) {
                            return Err("tensor_log requires strictly positive values".into());
                        }
                        t.data.iter().map(|x| x.ln()).collect()
                    }
                };
                let op = match n {
                    "tensor_sigmoid" => TensorOp::Sigmoid(id),
                    "tensor_tanh" => TensorOp::Tanh(id),
                    "tensor_exp" => TensorOp::Exp(id),
                    _ => TensorOp::Log(id),
                };
                tensor_store(data, t.shape, t.requires_grad, op)?
            }
            "tensor_reshape" => {
                if a.len() != 2 {
                    return Err("tensor_reshape expects tensor,shape".into());
                }
                let id = tensor_id(&a[0])?;
                let t = tensor_snapshot(id)?;
                let dims = as_num_list(&a[1])?;
                let mut shape = Vec::with_capacity(dims.len());
                for d in dims {
                    if d < 0.0 || d.fract() != 0.0 {
                        return Err(
                            "tensor_reshape dimensions must be non-negative integers".into()
                        );
                    }
                    shape.push(d as usize);
                }
                let size = if shape.is_empty() {
                    1
                } else {
                    shape.iter().product()
                };
                if size != t.data.len() {
                    return Err("tensor_reshape element count mismatch".into());
                }
                tensor_store(t.data, shape, t.requires_grad, TensorOp::Reshape(id))?
            }
            "tensor_transpose" => {
                if a.len() != 1 {
                    return Err("tensor_transpose expects tensor".into());
                }
                let id = tensor_id(&a[0])?;
                let t = tensor_snapshot(id)?;
                if t.shape.len() != 2 {
                    return Err("tensor_transpose expects a 2D tensor".into());
                }
                let (rows, cols) = (t.shape[0], t.shape[1]);
                let mut out = vec![0.0; t.data.len()];
                for r in 0..rows {
                    for c in 0..cols {
                        out[c * rows + r] = t.data[r * cols + c];
                    }
                }
                tensor_store(
                    out,
                    vec![cols, rows],
                    t.requires_grad,
                    TensorOp::Transpose(id, rows, cols),
                )?
            }
            "tensor_save" => {
                if a.len() != 2 {
                    return Err("tensor_save expects tensor,path".into());
                }
                let id = tensor_id(&a[0])?;
                let path = match &a[1] {
                    Val::Str(s) => s,
                    _ => return Err("tensor_save path must be String".into()),
                };
                let t = tensor_snapshot(id)?;
                let body = serde_json::json!({
                    "format": "veyra.tensor.v1",
                    "shape": t.shape,
                    "data": t.data,
                    "requires_grad": t.requires_grad
                });
                fs::write(
                    path,
                    serde_json::to_vec_pretty(&body).map_err(|e| e.to_string())?,
                )
                .map_err(|e| e.to_string())?;
                Val::Null
            }
            "tensor_load" => {
                if a.len() != 1 {
                    return Err("tensor_load expects path".into());
                }
                let path = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("tensor_load path must be String".into()),
                };
                let body: JsonValue =
                    serde_json::from_slice(&fs::read(path).map_err(|e| e.to_string())?)
                        .map_err(|e| e.to_string())?;
                if body.get("format").and_then(JsonValue::as_str) != Some("veyra.tensor.v1") {
                    return Err("unsupported tensor file format".into());
                }
                let shape = body
                    .get("shape")
                    .and_then(JsonValue::as_array)
                    .ok_or("tensor file missing shape")?
                    .iter()
                    .map(|v| v.as_u64().map(|n| n as usize).ok_or("invalid tensor shape"))
                    .collect::<Result<Vec<_>, _>>()?;
                let data = body
                    .get("data")
                    .and_then(JsonValue::as_array)
                    .ok_or("tensor file missing data")?
                    .iter()
                    .map(|v| v.as_f64().ok_or("invalid tensor value"))
                    .collect::<Result<Vec<_>, _>>()?;
                let requires_grad = body
                    .get("requires_grad")
                    .and_then(JsonValue::as_bool)
                    .unwrap_or(false);
                let size = if shape.is_empty() {
                    1
                } else {
                    shape.iter().product()
                };
                if size != data.len() {
                    return Err("tensor file shape/data mismatch".into());
                }
                tensor_store(data, shape, requires_grad, TensorOp::Leaf)?
            }
            "tensor_sum" | "tensor_mean" => {
                if a.len() != 1 {
                    return Err(format!("{n} expects tensor"));
                }
                let id = tensor_id(&a[0])?;
                let t = tensor_snapshot(id)?;
                let sum: f64 = t.data.iter().sum();
                let (value, op) = if n == "tensor_mean" {
                    (
                        if t.data.is_empty() {
                            0.0
                        } else {
                            sum / t.data.len() as f64
                        },
                        TensorOp::Mean(id),
                    )
                } else {
                    (sum, TensorOp::Sum(id))
                };
                tensor_store(vec![value], vec![], t.requires_grad, op)?
            }
            "tensor_matmul" => {
                if a.len() != 2 {
                    return Err("tensor_matmul expects two tensors".into());
                }
                let left_id = tensor_id(&a[0])?;
                let right_id = tensor_id(&a[1])?;
                let left = tensor_snapshot(left_id)?;
                let right = tensor_snapshot(right_id)?;
                if left.shape.len() != 2 || right.shape.len() != 2 {
                    return Err("tensor_matmul expects 2D tensors".into());
                }
                let (m, k) = (left.shape[0], left.shape[1]);
                let (k2, ncols) = (right.shape[0], right.shape[1]);
                if k != k2 {
                    return Err("tensor_matmul shape mismatch".into());
                }
                let mut out = vec![0.0; m * ncols];
                for i in 0..m {
                    for p in 0..k {
                        let a_ip = left.data[i * k + p];
                        let out_row = &mut out[i * ncols..(i + 1) * ncols];
                        let r_row = &right.data[p * ncols..(p + 1) * ncols];
                        for j in 0..ncols {
                            out_row[j] += a_ip * r_row[j];
                        }
                    }
                }
                tensor_store(
                    out,
                    vec![m, ncols],
                    left.requires_grad || right.requires_grad,
                    TensorOp::MatMul(left_id, right_id, m, k, ncols),
                )?
            }
            "tensor_backward" => {
                if a.len() != 1 {
                    return Err("tensor_backward expects scalar tensor".into());
                }
                let id = tensor_id(&a[0])?;
                tensor_backward(id)?;
                Val::Null
            }
            "tensor_step" => {
                if a.len() != 2 {
                    return Err("tensor_step expects tensor,learning_rate".into());
                }
                let id = tensor_id(&a[0])?;
                let lr = match a[1] {
                    Val::Num(n) => n,
                    _ => return Err("learning_rate must be Number".into()),
                };
                let mut store = tensors().lock().map_err(|_| "tensor registry poisoned")?;
                let t = store.get_mut(&id).ok_or("unknown tensor")?;
                if !t.requires_grad {
                    return Err("tensor_step requires trainable tensor".into());
                }
                for i in 0..t.data.len() {
                    t.data[i] -= lr * t.grad[i];
                }
                Val::Null
            }
            "tensor_adam_step" => {
                if a.len() != 5 {
                    return Err("tensor_adam_step expects tensor,lr,beta1,beta2,epsilon".into());
                }
                let id = tensor_id(&a[0])?;
                let number = |v: &Val, name: &str| match v {
                    Val::Num(n) => Ok(*n),
                    _ => Err(format!("{name} must be Number")),
                };
                let lr = number(&a[1], "learning_rate")?;
                let beta1 = number(&a[2], "beta1")?;
                let beta2 = number(&a[3], "beta2")?;
                let epsilon = number(&a[4], "epsilon")?;
                if !(0.0..1.0).contains(&beta1)
                    || !(0.0..1.0).contains(&beta2)
                    || lr <= 0.0
                    || epsilon <= 0.0
                {
                    return Err("invalid Adam hyperparameters".into());
                }
                let tensor = tensor_snapshot(id)?;
                if !tensor.requires_grad {
                    return Err("tensor_adam_step requires trainable tensor".into());
                }
                let updates = {
                    let mut states = adam_states().lock().map_err(|_| "adam state poisoned")?;
                    let state = states.entry(id).or_insert_with(|| AdamState {
                        m: vec![0.0; tensor.data.len()],
                        v: vec![0.0; tensor.data.len()],
                        step: 0,
                    });
                    if state.m.len() != tensor.data.len() {
                        *state = AdamState {
                            m: vec![0.0; tensor.data.len()],
                            v: vec![0.0; tensor.data.len()],
                            step: 0,
                        };
                    }
                    state.step += 1;
                    let bias1 = 1.0 - beta1.powf(state.step as f64);
                    let bias2 = 1.0 - beta2.powf(state.step as f64);
                    let mut updates = vec![0.0; tensor.data.len()];
                    for i in 0..tensor.data.len() {
                        let g = tensor.grad[i];
                        state.m[i] = beta1 * state.m[i] + (1.0 - beta1) * g;
                        state.v[i] = beta2 * state.v[i] + (1.0 - beta2) * g * g;
                        let m_hat = state.m[i] / bias1;
                        let v_hat = state.v[i] / bias2;
                        updates[i] = lr * m_hat / (v_hat.sqrt() + epsilon);
                    }
                    updates
                };
                let mut store = tensors().lock().map_err(|_| "tensor registry poisoned")?;
                let t = store.get_mut(&id).ok_or("unknown tensor")?;
                for (value, update) in t.data.iter_mut().zip(updates) {
                    *value -= update;
                }
                Val::Null
            }
            "tensor_reset_optimizer" => {
                if a.len() != 1 {
                    return Err("tensor_reset_optimizer expects tensor".into());
                }
                let id = tensor_id(&a[0])?;
                adam_states()
                    .lock()
                    .map_err(|_| "adam state poisoned")?
                    .remove(&id);
                Val::Null
            }
            "tensor_zero_grad" => {
                if a.len() != 1 {
                    return Err("tensor_zero_grad expects tensor".into());
                }
                let id = tensor_id(&a[0])?;
                {
                    let mut store = tensors().lock().map_err(|_| "tensor registry poisoned")?;
                    let t = store.get_mut(&id).ok_or("unknown tensor")?;
                    for g in &mut t.grad {
                        *g = 0.0;
                    }
                }
                let _ = self.gc_tensors()?;
                Val::Null
            }
            "tensor_gc" => {
                if !a.is_empty() {
                    return Err("tensor_gc expects 0 arguments".into());
                }
                Val::Num(self.gc_tensors()? as f64)
            }
            "tensor_live_count" => {
                if !a.is_empty() {
                    return Err("tensor_live_count expects 0 arguments".into());
                }
                let count = tensors()
                    .lock()
                    .map_err(|_| "tensor registry poisoned")?
                    .len();
                Val::Num(count as f64)
            }
            "accuracy_score" => {
                if a.len() != 2 {
                    return Err("accuracy_score expects actual,predicted".into());
                }
                let actual = match &a[0] {
                    Val::List(v) => v,
                    _ => return Err("actual must be List".into()),
                };
                let predicted = match &a[1] {
                    Val::List(v) => v,
                    _ => return Err("predicted must be List".into()),
                };
                if actual.len() != predicted.len() || actual.is_empty() {
                    return Err("accuracy_score length mismatch".into());
                }
                let correct = actual
                    .iter()
                    .zip(predicted.iter())
                    .filter(|(x, y)| x == y)
                    .count();
                Val::Num(correct as f64 / actual.len() as f64)
            }
            "classification_metrics" => {
                if a.len() != 2 {
                    return Err("classification_metrics expects actual,predicted".into());
                }
                let actual = match &a[0] {
                    Val::List(v) => v,
                    _ => return Err("actual must be List".into()),
                };
                let predicted = match &a[1] {
                    Val::List(v) => v,
                    _ => return Err("predicted must be List".into()),
                };
                if actual.len() != predicted.len() || actual.is_empty() {
                    return Err("classification_metrics length mismatch".into());
                }
                let positive =
                    |v: &Val| matches!(v,Val::Num(n) if *n==1.0) || matches!(v, Val::Bool(true));
                let mut tp = 0.0;
                let mut tn = 0.0;
                let mut fp = 0.0;
                let mut fnn = 0.0;
                for (x, y) in actual.iter().zip(predicted.iter()) {
                    match (positive(x), positive(y)) {
                        (true, true) => tp += 1.0,
                        (false, false) => tn += 1.0,
                        (false, true) => fp += 1.0,
                        (true, false) => fnn += 1.0,
                    }
                }
                let precision = if tp + fp == 0.0 { 0.0 } else { tp / (tp + fp) };
                let recall = if tp + fnn == 0.0 {
                    0.0
                } else {
                    tp / (tp + fnn)
                };
                let f1 = if precision + recall == 0.0 {
                    0.0
                } else {
                    2.0 * precision * recall / (precision + recall)
                };
                let accuracy = (tp + tn) / (tp + tn + fp + fnn);
                Val::Map(vec![
                    (Val::Str("accuracy".into()), Val::Num(accuracy)),
                    (Val::Str("precision".into()), Val::Num(precision)),
                    (Val::Str("recall".into()), Val::Num(recall)),
                    (Val::Str("f1".into()), Val::Num(f1)),
                    (Val::Str("tp".into()), Val::Num(tp)),
                    (Val::Str("tn".into()), Val::Num(tn)),
                    (Val::Str("fp".into()), Val::Num(fp)),
                    (Val::Str("fn".into()), Val::Num(fnn)),
                ])
            }
            "train_test_split" => {
                if a.len() != 3 {
                    return Err("train_test_split expects x,y,test_ratio".into());
                }
                let x = match &a[0] {
                    Val::List(v) => v,
                    _ => return Err("x must be List".into()),
                };
                let y = match &a[1] {
                    Val::List(v) => v,
                    _ => return Err("y must be List".into()),
                };
                let ratio = match a[2] {
                    Val::Num(n) => n,
                    _ => return Err("test_ratio must be Number".into()),
                };
                if x.len() != y.len() || x.is_empty() {
                    return Err("train_test_split length mismatch".into());
                }
                if !(0.0..1.0).contains(&ratio) {
                    return Err("test_ratio must be between 0 and 1".into());
                }
                let test_count = ((x.len() as f64) * ratio).round().max(1.0) as usize;
                let split = x.len().saturating_sub(test_count.min(x.len()));
                Val::Map(vec![
                    (Val::Str("train_x".into()), Val::List(x[..split].to_vec())),
                    (Val::Str("train_y".into()), Val::List(y[..split].to_vec())),
                    (Val::Str("test_x".into()), Val::List(x[split..].to_vec())),
                    (Val::Str("test_y".into()), Val::List(y[split..].to_vec())),
                ])
            }
            "mse" => {
                if a.len() != 2 {
                    return Err("mse expects actual,predicted".into());
                }
                let x = as_num_list(&a[0])?;
                let y = as_num_list(&a[1])?;
                if x.len() != y.len() || x.is_empty() {
                    return Err("mse length mismatch".into());
                }
                Val::Num(
                    x.iter()
                        .zip(y.iter())
                        .map(|(a, b)| (a - b) * (a - b))
                        .sum::<f64>()
                        / x.len() as f64,
                )
            }
            "normalize" => {
                if a.len() != 1 {
                    return Err("normalize expects list".into());
                }
                let xs = as_num_list(&a[0])?;
                if xs.is_empty() {
                    return Err("normalize requires non-empty list".into());
                }
                let min = xs.iter().fold(f64::INFINITY, |m, x| m.min(*x));
                let max = xs.iter().fold(f64::NEG_INFINITY, |m, x| m.max(*x));
                let span = max - min;
                if span == 0.0 {
                    nums_to_val(vec![0.0; xs.len()])
                } else {
                    nums_to_val(xs.into_iter().map(|x| (x - min) / span).collect())
                }
            }
            "relu" => {
                if a.len() != 1 {
                    return Err("relu expects list".into());
                }
                nums_to_val(
                    as_num_list(&a[0])?
                        .into_iter()
                        .map(|x| x.max(0.0))
                        .collect(),
                )
            }
            "softmax" => {
                if a.len() != 1 {
                    return Err("softmax expects list".into());
                }
                let xs = as_num_list(&a[0])?;
                if xs.is_empty() {
                    return Err("softmax requires non-empty list".into());
                }
                let m = xs.iter().fold(f64::NEG_INFINITY, |m, x| m.max(*x));
                let exps: Vec<f64> = xs.iter().map(|x| (x - m).exp()).collect();
                let sum: f64 = exps.iter().sum();
                nums_to_val(exps.into_iter().map(|x| x / sum).collect())
            }
            "dense" => {
                if a.len() != 3 {
                    return Err("dense expects input,weights,bias".into());
                }
                let input = as_num_list(&a[0])?;
                let weights = as_matrix(&a[1])?;
                let bias = as_num_list(&a[2])?;
                if weights.len() != bias.len() || weights.iter().any(|r| r.len() != input.len()) {
                    return Err("dense dimension mismatch".into());
                }
                nums_to_val(
                    weights
                        .iter()
                        .zip(bias.iter())
                        .map(|(row, b)| {
                            row.iter()
                                .zip(input.iter())
                                .map(|(w, x)| w * x)
                                .sum::<f64>()
                                + b
                        })
                        .collect(),
                )
            }
            "kmeans" => {
                if a.len() != 3 {
                    return Err("kmeans expects data,k,iterations".into());
                }
                let data = as_matrix(&a[0])?;
                let k = match a[1] {
                    Val::Num(n) => n as usize,
                    _ => return Err("k must be Number".into()),
                };
                let iters = match a[2] {
                    Val::Num(n) => n as usize,
                    _ => return Err("iterations must be Number".into()),
                };
                if data.is_empty() || k == 0 || k > data.len() {
                    return Err("invalid kmeans data".into());
                }
                let dims = data[0].len();
                if dims == 0 || data.iter().any(|r| r.len() != dims) {
                    return Err("ragged kmeans data".into());
                }
                let mut centers = data[..k].to_vec();
                for _ in 0..iters {
                    let mut sums = vec![vec![0.0; dims]; k];
                    let mut counts = vec![0usize; k];
                    for point in &data {
                        let mut best = 0usize;
                        let mut bestd = distance_sq(point, &centers[0]);
                        for i in 1..k {
                            let dist = distance_sq(point, &centers[i]);
                            if dist < bestd {
                                best = i;
                                bestd = dist;
                            }
                        }
                        counts[best] += 1;
                        for j in 0..dims {
                            sums[best][j] += point[j];
                        }
                    }
                    for i in 0..k {
                        if counts[i] > 0 {
                            for j in 0..dims {
                                centers[i][j] = sums[i][j] / counts[i] as f64;
                            }
                        }
                    }
                }
                matrix_to_val(centers)
            }
            _ => return Ok(None),
        };
        Ok(Some(v))
    }
}
