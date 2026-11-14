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
    AdamState, Conv2dDims, TensorOp, adam_states, broadcast_index, broadcast_shape, conv2d_forward,
    layer_norm_stats, softmax_rows, tensor_backward, tensor_default_device, tensor_from_value,
    tensor_id, tensor_move_device, tensor_set_default_device, tensor_snapshot, tensor_store,
    tensor_store_on_device, tensors,
};
use crate::value::{
    Val, as_matrix, as_num_list, constant_time_strings_equal, distance_sq, hmac_sha256_hex,
    json_to_val, map_lookup, matrix_determinant, matrix_inverse, matrix_to_val, nums_to_val,
    pbkdf2_sha256_hex, solve_linear_system, svg_escape, val_to_json,
};
#[cfg(feature = "native-services")]
use crate::value::{json_stringify_value, val_to_sql_value};
use crate::vm::{Vm, collect_generator, generators, resume_generator};

fn parse_pos_int(arg: Option<&Val>, default: usize, label: &str) -> Result<usize, String> {
    match arg {
        None => Ok(default),
        Some(Val::Num(n)) if *n >= 1.0 && n.fract() == 0.0 => Ok(*n as usize),
        _ => Err(format!("{label} must be a positive integer")),
    }
}

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
                let use_gpu = (left.device == "gpu" || right.device == "gpu")
                    && left.shape == right.shape
                    && n != "tensor_div";
                let data = if use_gpu {
                    let op_code = match n {
                        "tensor_add" => 0,
                        "tensor_sub" => 1,
                        _ => 2,
                    };
                    crate::gpu::gpu_ewise_f64(&left.data, &right.data, op_code, 0.0)
                } else {
                    let mut buf = Vec::with_capacity(out_len);
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
                        buf.push(v);
                    }
                    buf
                };
                let requires_grad = left.requires_grad || right.requires_grad;
                let op = match n {
                    "tensor_add" => TensorOp::Add(left_id, right_id),
                    "tensor_sub" => TensorOp::Sub(left_id, right_id),
                    "tensor_mul" => TensorOp::Mul(left_id, right_id),
                    _ => TensorOp::Div(left_id, right_id),
                };
                let target_dev = if use_gpu {
                    "gpu"
                } else {
                    tensor_default_device()
                };
                tensor_store_on_device(data, out_shape, requires_grad, op, target_dev)?
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
                let out = if t.device == "gpu" {
                    crate::gpu::gpu_softmax_rows_f64(&t.data, rows, cols)
                } else {
                    softmax_rows(&t.data, rows, cols)
                };
                tensor_store_on_device(
                    out,
                    t.shape,
                    t.requires_grad,
                    TensorOp::SoftmaxRows(id, rows, cols),
                    &t.device,
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
                let data = if t.device == "gpu" {
                    crate::gpu::gpu_ewise_f64(&t.data, &[], 3, 0.0)
                } else {
                    t.data.iter().map(|x| x.max(0.0)).collect()
                };
                tensor_store_on_device(
                    data,
                    t.shape,
                    t.requires_grad,
                    TensorOp::Relu(id),
                    &t.device,
                )?
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
                let use_gpu = left.device == "gpu" || right.device == "gpu";
                let out = if use_gpu {
                    crate::gpu::gpu_matmul_f64(&left.data, &right.data, m, k, ncols)
                } else {
                    let mut buf = vec![0.0; m * ncols];
                    let ops = m.saturating_mul(k).saturating_mul(ncols);
                    if ops >= 32_768 && m >= 8 {
                        let threads = thread::available_parallelism()
                            .map(|n| n.get())
                            .unwrap_or(4)
                            .min(m);
                        let rows_per_chunk = m.div_ceil(threads);
                        let l_data = &left.data;
                        let r_data = &right.data;
                        thread::scope(|s| {
                            for (chunk_idx, out_chunk) in
                                buf.chunks_mut(rows_per_chunk * ncols).enumerate()
                            {
                                let row_start = chunk_idx * rows_per_chunk;
                                let chunk_rows = out_chunk.len() / ncols;
                                s.spawn(move || {
                                    for local_i in 0..chunk_rows {
                                        let i = row_start + local_i;
                                        let out_row =
                                            &mut out_chunk[local_i * ncols..(local_i + 1) * ncols];
                                        for p in 0..k {
                                            let a_ip = l_data[i * k + p];
                                            let r_row = &r_data[p * ncols..(p + 1) * ncols];
                                            for j in 0..ncols {
                                                out_row[j] += a_ip * r_row[j];
                                            }
                                        }
                                    }
                                });
                            }
                        });
                    } else {
                        for i in 0..m {
                            for p in 0..k {
                                let a_ip = left.data[i * k + p];
                                let out_row = &mut buf[i * ncols..(i + 1) * ncols];
                                let r_row = &right.data[p * ncols..(p + 1) * ncols];
                                for j in 0..ncols {
                                    out_row[j] += a_ip * r_row[j];
                                }
                            }
                        }
                    }
                    buf
                };
                let dev = if use_gpu {
                    "gpu"
                } else {
                    tensor_default_device()
                };
                tensor_store_on_device(
                    out,
                    vec![m, ncols],
                    left.requires_grad || right.requires_grad,
                    TensorOp::MatMul(left_id, right_id, m, k, ncols),
                    dev,
                )?
            }
            "tensor_scale" => {
                if a.len() != 2 {
                    return Err("tensor_scale expects tensor,factor".into());
                }
                let id = tensor_id(&a[0])?;
                let k = match a[1] {
                    Val::Num(n) => n,
                    _ => return Err("tensor_scale factor must be Number".into()),
                };
                let t = tensor_snapshot(id)?;
                let data = t.data.iter().map(|x| x * k).collect();
                tensor_store(data, t.shape, t.requires_grad, TensorOp::Scale(id, k))?
            }
            "tensor_cross_entropy" => {
                if a.len() != 2 {
                    return Err("tensor_cross_entropy expects logits,targets".into());
                }
                let id = tensor_id(&a[0])?;
                let t = tensor_snapshot(id)?;
                let (rows, cols) = match t.shape.as_slice() {
                    [c] => (1usize, *c),
                    [r, c] => (*r, *c),
                    _ => return Err("tensor_cross_entropy expects 1D or 2D logits".into()),
                };
                let targets = as_num_list(&a[1])?;
                if targets.len() != rows {
                    return Err(format!(
                        "tensor_cross_entropy expects {rows} targets, got {}",
                        targets.len()
                    ));
                }
                let mut classes = Vec::with_capacity(rows);
                for v in targets {
                    if v < 0.0 || v.fract() != 0.0 || v as usize >= cols {
                        return Err(format!("target class {v} out of range 0..{cols}"));
                    }
                    classes.push(v as usize);
                }
                let probs = softmax_rows(&t.data, rows, cols);
                let loss = (0..rows)
                    .map(|r| -probs[r * cols + classes[r]].max(1e-300).ln())
                    .sum::<f64>()
                    / rows as f64;
                tensor_store(
                    vec![loss],
                    vec![],
                    t.requires_grad,
                    TensorOp::CrossEntropy(id, classes, rows, cols),
                )?
            }
            "tensor_layer_norm" => {
                if a.is_empty() || a.len() > 2 {
                    return Err("tensor_layer_norm expects tensor[,epsilon]".into());
                }
                let id = tensor_id(&a[0])?;
                let eps = match a.get(1) {
                    None => 1e-5,
                    Some(Val::Num(n)) if *n > 0.0 => *n,
                    _ => return Err("tensor_layer_norm epsilon must be positive Number".into()),
                };
                let t = tensor_snapshot(id)?;
                let cols = *t
                    .shape
                    .last()
                    .ok_or("tensor_layer_norm expects non-scalar")?;
                if cols == 0 {
                    return Err("tensor_layer_norm last dimension must be non-empty".into());
                }
                let rows = t.data.len() / cols;
                let stats = layer_norm_stats(&t.data, rows, cols, eps);
                let mut out = vec![0.0; t.data.len()];
                for r in 0..rows {
                    let (mean, inv_std) = stats[r];
                    for c in 0..cols {
                        out[r * cols + c] = (t.data[r * cols + c] - mean) * inv_std;
                    }
                }
                tensor_store(
                    out,
                    t.shape,
                    t.requires_grad,
                    TensorOp::LayerNorm(id, rows, cols, eps),
                )?
            }
            "tensor_conv2d" => {
                if a.len() < 2 || a.len() > 3 {
                    return Err("tensor_conv2d expects input,weight[,stride]".into());
                }
                let in_id = tensor_id(&a[0])?;
                let w_id = tensor_id(&a[1])?;
                let stride = parse_pos_int(a.get(2), 1, "tensor_conv2d stride")?;
                let input = tensor_snapshot(in_id)?;
                let weight = tensor_snapshot(w_id)?;
                let (n, c, h, w) = match input.shape.as_slice() {
                    [n, c, h, w] => (*n, *c, *h, *w),
                    _ => return Err("tensor_conv2d input must be 4D [N,C,H,W]".into()),
                };
                let (o, wc, kh, kw) = match weight.shape.as_slice() {
                    [o, c, kh, kw] => (*o, *c, *kh, *kw),
                    _ => return Err("tensor_conv2d weight must be 4D [O,C,KH,KW]".into()),
                };
                if wc != c || kh > h || kw > w || kh == 0 || kw == 0 {
                    return Err("tensor_conv2d shape mismatch".into());
                }
                let dims = Conv2dDims {
                    n,
                    c,
                    h,
                    w,
                    o,
                    kh,
                    kw,
                    stride,
                    oh: (h - kh) / stride + 1,
                    ow: (w - kw) / stride + 1,
                };
                let use_gpu = input.device == "gpu" || weight.device == "gpu";
                let out = if use_gpu {
                    crate::gpu::gpu_conv2d_f64(&input.data, &weight.data, &dims)
                } else {
                    conv2d_forward(&input.data, &weight.data, &dims)
                };
                let dev = if use_gpu {
                    "gpu"
                } else {
                    tensor_default_device()
                };
                tensor_store_on_device(
                    out,
                    vec![n, o, dims.oh, dims.ow],
                    input.requires_grad || weight.requires_grad,
                    TensorOp::Conv2d(in_id, w_id, dims),
                    dev,
                )?
            }
            "tensor_conv1d" => {
                if a.len() < 2 || a.len() > 3 {
                    return Err("tensor_conv1d expects input,weight[,stride]".into());
                }
                let in_id = tensor_id(&a[0])?;
                let w_id = tensor_id(&a[1])?;
                let stride = parse_pos_int(a.get(2), 1, "tensor_conv1d stride")?;
                let input = tensor_snapshot(in_id)?;
                let weight = tensor_snapshot(w_id)?;
                let (n, c, l) = match input.shape.as_slice() {
                    [n, c, l] => (*n, *c, *l),
                    _ => return Err("tensor_conv1d input must be 3D [N,C,L]".into()),
                };
                let (o, wc, k) = match weight.shape.as_slice() {
                    [o, c, k] => (*o, *c, *k),
                    _ => return Err("tensor_conv1d weight must be 3D [O,C,K]".into()),
                };
                if wc != c || k > l || k == 0 {
                    return Err("tensor_conv1d shape mismatch".into());
                }
                let dims = Conv2dDims {
                    n,
                    c,
                    h: 1,
                    w: l,
                    o,
                    kh: 1,
                    kw: k,
                    stride,
                    oh: 1,
                    ow: (l - k) / stride + 1,
                };
                let out = conv2d_forward(&input.data, &weight.data, &dims);
                tensor_store(
                    out,
                    vec![n, o, dims.ow],
                    input.requires_grad || weight.requires_grad,
                    TensorOp::Conv2d(in_id, w_id, dims),
                )?
            }
            "tensor_attention" => {
                if a.len() != 3 {
                    return Err("tensor_attention expects q,k,v".into());
                }
                let q = tensor_snapshot(tensor_id(&a[0])?)?;
                if q.shape.len() != 2 {
                    return Err("tensor_attention expects 2D [seq, dim] tensors".into());
                }
                let d = q.shape[1].max(1) as f64;
                let mut call = |name: &str, args: Vec<Val>| -> Result<Val, String> {
                    self.call_builtin_science_ml(name, args)?
                        .ok_or_else(|| format!("internal: missing builtin {name}"))
                };
                let kt = call("tensor_transpose", vec![a[1].clone()])?;
                let scores = call("tensor_matmul", vec![a[0].clone(), kt])?;
                let scaled = call("tensor_scale", vec![scores, Val::Num(1.0 / d.sqrt())])?;
                let weights = call("tensor_softmax", vec![scaled])?;
                call("tensor_matmul", vec![weights, a[2].clone()])?
            }
            "tensor_multihead_attention" => {
                if a.len() != 4 {
                    return Err("tensor_multihead_attention expects q,k,v,num_heads".into());
                }
                let num_heads =
                    parse_pos_int(Some(&a[3]), 1, "tensor_multihead_attention num_heads")?;
                let q_id = tensor_id(&a[0])?;
                let k_id = tensor_id(&a[1])?;
                let v_id = tensor_id(&a[2])?;
                let q = tensor_snapshot(q_id)?;
                let k = tensor_snapshot(k_id)?;
                let v = tensor_snapshot(v_id)?;
                if q.shape.len() != 2 || k.shape.len() != 2 || v.shape.len() != 2 {
                    return Err("tensor_multihead_attention expects 2D [seq, dim] tensors".into());
                }
                let (seq_q, d_model) = (q.shape[0], q.shape[1]);
                let seq_k = k.shape[0];
                if k.shape[1] != d_model || v.shape[1] != d_model || v.shape[0] != seq_k {
                    return Err("tensor_multihead_attention shape mismatch".into());
                }
                if d_model == 0 || d_model % num_heads != 0 {
                    return Err(format!(
                        "tensor_multihead_attention dim {d_model} must be divisible by {num_heads}"
                    ));
                }
                let head_dim = d_model / num_heads;
                let slice_cols = |data: &[f64],
                                  req_grad: bool,
                                  id: u64,
                                  rows: usize,
                                  start: usize|
                 -> Result<Val, String> {
                    let mut sliced = Vec::with_capacity(rows * head_dim);
                    for r in 0..rows {
                        sliced.extend_from_slice(
                            &data[r * d_model + start..r * d_model + start + head_dim],
                        );
                    }
                    tensor_store(
                        sliced,
                        vec![rows, head_dim],
                        req_grad,
                        TensorOp::SliceCols(id, rows, d_model, start, head_dim),
                    )
                };
                let mut head_ids = Vec::with_capacity(num_heads);
                let mut head_snaps = Vec::with_capacity(num_heads);
                for h in 0..num_heads {
                    let start = h * head_dim;
                    let qh = slice_cols(&q.data, q.requires_grad, q_id, seq_q, start)?;
                    let kh = slice_cols(&k.data, k.requires_grad, k_id, seq_k, start)?;
                    let vh = slice_cols(&v.data, v.requires_grad, v_id, seq_k, start)?;
                    let head_val = self
                        .call_builtin_science_ml("tensor_attention", vec![qh, kh, vh])?
                        .ok_or_else(|| "internal: missing builtin tensor_attention".to_string())?;
                    let hid = tensor_id(&head_val)?;
                    head_ids.push(hid);
                    head_snaps.push(tensor_snapshot(hid)?);
                }
                let mut out = vec![0.0; seq_q * d_model];
                let mut any_grad = false;
                for (h, snap) in head_snaps.iter().enumerate() {
                    any_grad |= snap.requires_grad;
                    let start = h * head_dim;
                    for r in 0..seq_q {
                        for c in 0..head_dim {
                            out[r * d_model + start + c] = snap.data[r * head_dim + c];
                        }
                    }
                }
                tensor_store(
                    out,
                    vec![seq_q, d_model],
                    any_grad,
                    TensorOp::ConcatCols(head_ids, seq_q, vec![head_dim; num_heads]),
                )?
            }
            "tensor_checkpoint_save" => {
                if a.len() != 2 {
                    return Err("tensor_checkpoint_save expects map,path".into());
                }
                let pairs = match &a[0] {
                    Val::Map(p) => p,
                    _ => return Err("tensor_checkpoint_save expects Map of name -> tensor".into()),
                };
                let path = match &a[1] {
                    Val::Str(s) => s,
                    _ => return Err("tensor_checkpoint_save path must be String".into()),
                };
                let mut entries = serde_json::Map::new();
                for (k, v) in pairs {
                    let name = match k {
                        Val::Str(s) => s.clone(),
                        _ => return Err("checkpoint keys must be Strings".into()),
                    };
                    let t = tensor_snapshot(tensor_id(v)?)?;
                    entries.insert(
                        name,
                        serde_json::json!({
                            "shape": t.shape,
                            "data": t.data,
                            "requires_grad": t.requires_grad
                        }),
                    );
                }
                let body = serde_json::json!({
                    "format": "veyra.checkpoint.v1",
                    "tensors": entries
                });
                fs::write(path, serde_json::to_vec(&body).map_err(|e| e.to_string())?)
                    .map_err(|e| e.to_string())?;
                Val::Num(pairs.len() as f64)
            }
            "tensor_checkpoint_load" => {
                if a.len() != 1 {
                    return Err("tensor_checkpoint_load expects path".into());
                }
                let path = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("tensor_checkpoint_load path must be String".into()),
                };
                let body: JsonValue =
                    serde_json::from_slice(&fs::read(path).map_err(|e| e.to_string())?)
                        .map_err(|e| e.to_string())?;
                if body.get("format").and_then(JsonValue::as_str) != Some("veyra.checkpoint.v1") {
                    return Err("unsupported checkpoint format".into());
                }
                let entries = body
                    .get("tensors")
                    .and_then(JsonValue::as_object)
                    .ok_or("checkpoint missing tensors")?;
                let mut out = Vec::with_capacity(entries.len());
                for (name, t) in entries {
                    let shape = t
                        .get("shape")
                        .and_then(JsonValue::as_array)
                        .ok_or("checkpoint tensor missing shape")?
                        .iter()
                        .map(|v| v.as_u64().map(|n| n as usize).ok_or("invalid shape"))
                        .collect::<Result<Vec<_>, _>>()?;
                    let data = t
                        .get("data")
                        .and_then(JsonValue::as_array)
                        .ok_or("checkpoint tensor missing data")?
                        .iter()
                        .map(|v| v.as_f64().ok_or("invalid tensor value"))
                        .collect::<Result<Vec<_>, _>>()?;
                    let size: usize = if shape.is_empty() {
                        1
                    } else {
                        shape.iter().product()
                    };
                    if size != data.len() {
                        return Err(format!("checkpoint tensor '{name}' shape/data mismatch"));
                    }
                    let requires_grad = t
                        .get("requires_grad")
                        .and_then(JsonValue::as_bool)
                        .unwrap_or(false);
                    out.push((
                        Val::Str(name.clone()),
                        tensor_store(data, shape, requires_grad, TensorOp::Leaf)?,
                    ));
                }
                Val::Map(out)
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
                    t.grad.fill(0.0);
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
            "tensor_to" => {
                if a.len() != 2 {
                    return Err("tensor_to expects tensor,device".into());
                }
                let id = tensor_id(&a[0])?;
                let dev = match &a[1] {
                    Val::Str(s) => s,
                    _ => return Err("tensor_to device must be String".into()),
                };
                tensor_move_device(id, dev)?
            }
            "tensor_device" => {
                if a.len() != 1 {
                    return Err("tensor_device expects tensor".into());
                }
                let id = tensor_id(&a[0])?;
                Val::Str(tensor_snapshot(id)?.device)
            }
            "tensor_default_device" => {
                if a.is_empty() {
                    Val::Str(tensor_default_device().to_string())
                } else if a.len() == 1 {
                    let Val::Str(dev) = &a[0] else {
                        return Err("tensor_default_device expects String device".into());
                    };
                    Val::Str(tensor_set_default_device(dev)?)
                } else {
                    return Err("tensor_default_device expects 0 or 1 argument".into());
                }
            }
            "gpu_available" => {
                if !a.is_empty() {
                    return Err("gpu_available expects 0 arguments".into());
                }
                Val::Bool(crate::gpu::gpu_available())
            }
            "gpu_info" => {
                if !a.is_empty() {
                    return Err("gpu_info expects 0 arguments".into());
                }
                crate::gpu::gpu_info_val()
            }
            "gpu_devices" => {
                if !a.is_empty() {
                    return Err("gpu_devices expects 0 arguments".into());
                }
                if let Val::Map(pairs) = crate::gpu::gpu_info_val() {
                    map_lookup(&pairs, &Val::Str("adapters".into()))
                        .cloned()
                        .unwrap_or(Val::List(Vec::new()))
                } else {
                    Val::List(Vec::new())
                }
            }
            "gpu_matmul" => {
                if a.len() != 2 {
                    return Err("gpu_matmul expects two matrices or 2D tensors".into());
                }
                if let (Ok(lid), Ok(rid)) = (tensor_id(&a[0]), tensor_id(&a[1])) {
                    let left = tensor_snapshot(lid)?;
                    let right = tensor_snapshot(rid)?;
                    if left.shape.len() != 2 || right.shape.len() != 2 {
                        return Err("gpu_matmul expects 2D tensors".into());
                    }
                    let (m, k) = (left.shape[0], left.shape[1]);
                    let (k2, ncols) = (right.shape[0], right.shape[1]);
                    if k != k2 {
                        return Err("gpu_matmul shape mismatch".into());
                    }
                    let out = crate::gpu::gpu_matmul_f64(&left.data, &right.data, m, k, ncols);
                    tensor_store_on_device(
                        out,
                        vec![m, ncols],
                        left.requires_grad || right.requires_grad,
                        TensorOp::MatMul(lid, rid, m, k, ncols),
                        "gpu",
                    )?
                } else {
                    let left = as_matrix(&a[0])?;
                    let right = as_matrix(&a[1])?;
                    let m = left.len();
                    let k = left[0].len();
                    if right.len() != k {
                        return Err("gpu_matmul shape mismatch".into());
                    }
                    let ncols = right[0].len();
                    let flat_a: Vec<f64> = left.into_iter().flatten().collect();
                    let flat_b: Vec<f64> = right.into_iter().flatten().collect();
                    let flat_c = crate::gpu::gpu_matmul_f64(&flat_a, &flat_b, m, k, ncols);
                    let rows = flat_c.chunks(ncols).map(|r| r.to_vec()).collect::<Vec<_>>();
                    matrix_to_val(rows)
                }
            }
            "gpu_vec_add" | "gpu_vec_sub" | "gpu_vec_mul" => {
                if a.len() != 2 {
                    return Err(format!("{n} expects two numeric lists"));
                }
                let xs = as_num_list(&a[0])?;
                let ys = as_num_list(&a[1])?;
                if xs.len() != ys.len() {
                    return Err(format!("{n} list length mismatch"));
                }
                let code = match n {
                    "gpu_vec_add" => 0,
                    "gpu_vec_sub" => 1,
                    _ => 2,
                };
                nums_to_val(crate::gpu::gpu_ewise_f64(&xs, &ys, code, 0.0))
            }
            "gpu_relu" | "gpu_sigmoid" => {
                if a.len() != 1 {
                    return Err(format!("{n} expects numeric list"));
                }
                let xs = as_num_list(&a[0])?;
                let code = if n == "gpu_relu" { 3 } else { 4 };
                nums_to_val(crate::gpu::gpu_ewise_f64(&xs, &[], code, 0.0))
            }
            "gpu_scale" => {
                if a.len() != 2 {
                    return Err("gpu_scale expects list,scalar".into());
                }
                let xs = as_num_list(&a[0])?;
                let Val::Num(k) = a[1] else {
                    return Err("gpu_scale scalar must be Number".into());
                };
                nums_to_val(crate::gpu::gpu_ewise_f64(&xs, &[], 5, k))
            }
            "gpu_softmax" => {
                if a.len() != 1 {
                    return Err("gpu_softmax expects matrix".into());
                }
                let mat = as_matrix(&a[0])?;
                let rows = mat.len();
                let cols = mat[0].len();
                let flat: Vec<f64> = mat.into_iter().flatten().collect();
                let out = crate::gpu::gpu_softmax_rows_f64(&flat, rows, cols);
                matrix_to_val(out.chunks(cols).map(|r| r.to_vec()).collect())
            }
            "gpu_run_hlsl" => {
                if a.len() < 5 || a.len() > 8 {
                    return Err(
                        "gpu_run_hlsl expects (source,entry,buf_a,buf_b,out_len[,gx,gy,gz])".into(),
                    );
                }
                let Val::Str(src) = &a[0] else {
                    return Err("hlsl source must be String".into());
                };
                let Val::Str(entry) = &a[1] else {
                    return Err("hlsl entry must be String".into());
                };
                let buf_a = as_num_list(&a[2])?;
                let buf_b = as_num_list(&a[3])?;
                let out_len = parse_pos_int(Some(&a[4]), 1, "gpu_run_hlsl out_len")?;
                let gx = parse_pos_int(a.get(5), out_len.div_ceil(64).max(1), "gx")? as u32;
                let gy = parse_pos_int(a.get(6), 1, "gy")? as u32;
                let gz = parse_pos_int(a.get(7), 1, "gz")? as u32;
                let out =
                    crate::gpu::gpu_run_hlsl(src, entry, &buf_a, &buf_b, out_len, gx, gy, gz)?;
                nums_to_val(out)
            }
            "gpu_backends" => {
                if !a.is_empty() {
                    return Err("gpu_backends expects 0 arguments".into());
                }
                Val::List(
                    crate::gpu::gpu_backends()
                        .into_iter()
                        .map(Val::Str)
                        .collect(),
                )
            }
            "gpu_set_backend" => {
                if a.len() != 1 {
                    return Err("gpu_set_backend expects backend name String".into());
                }
                let Val::Str(name) = &a[0] else {
                    return Err("gpu_set_backend expects String".into());
                };
                Val::Str(crate::gpu::gpu_set_backend(name)?)
            }
            "gpu_precision" => {
                if !a.is_empty() {
                    return Err("gpu_precision expects 0 arguments".into());
                }
                Val::Str(crate::gpu::gpu_precision().into())
            }
            "gpu_set_precision" => {
                if a.len() != 1 {
                    return Err(
                        "gpu_set_precision expects precision String ('f32' or 'f64')".into(),
                    );
                }
                let Val::Str(name) = &a[0] else {
                    return Err("gpu_set_precision expects String".into());
                };
                Val::Str(crate::gpu::gpu_set_precision(name)?)
            }
            "gpu_gelu" => {
                if a.len() != 1 {
                    return Err("gpu_gelu expects numeric list or tensor".into());
                }
                if let Ok(id) = tensor_id(&a[0]) {
                    let t = tensor_snapshot(id)?;
                    let out = crate::gpu::gpu_ewise_f64(&t.data, &[], 6, 0.0);
                    tensor_store_on_device(out, t.shape, false, TensorOp::Leaf, "gpu")?
                } else {
                    let xs = as_num_list(&a[0])?;
                    nums_to_val(crate::gpu::gpu_ewise_f64(&xs, &[], 6, 0.0))
                }
            }
            "gpu_layernorm" => {
                if a.is_empty() || a.len() > 2 {
                    return Err("gpu_layernorm expects (data_or_tensor[, eps])".into());
                }
                let eps = match a.get(1) {
                    None => 1e-5,
                    Some(Val::Num(e)) if *e > 0.0 && e.is_finite() => *e,
                    _ => return Err("gpu_layernorm eps must be a positive finite Number".into()),
                };
                if let Ok(id) = tensor_id(&a[0]) {
                    let t = tensor_snapshot(id)?;
                    let (rows, cols) = match t.shape.as_slice() {
                        [n] => (1usize, *n),
                        [r, c] => (*r, *c),
                        _ => return Err("gpu_layernorm expects 1D or 2D tensor".into()),
                    };
                    let out = crate::gpu::gpu_layernorm_f64(&t.data, rows, cols, eps);
                    tensor_store_on_device(out, t.shape, false, TensorOp::Leaf, "gpu")?
                } else if let Ok(mat) = as_matrix(&a[0]) {
                    let rows = mat.len();
                    let cols = mat[0].len();
                    let flat: Vec<f64> = mat.into_iter().flatten().collect();
                    let out = crate::gpu::gpu_layernorm_f64(&flat, rows, cols, eps);
                    matrix_to_val(out.chunks(cols).map(|r| r.to_vec()).collect())
                } else {
                    let xs = as_num_list(&a[0])?;
                    if xs.is_empty() {
                        return Err("gpu_layernorm requires non-empty input".into());
                    }
                    let cols = xs.len();
                    nums_to_val(crate::gpu::gpu_layernorm_f64(&xs, 1, cols, eps))
                }
            }
            "gpu_run_spirv" => {
                if a.len() < 4 || a.len() > 8 {
                    return Err(
                        "gpu_run_spirv expects (words,buf_a,buf_b,out_len[,push_words,gx,gy,gz])"
                            .into(),
                    );
                }
                let parse_u32s = |val: &Val, label: &str| -> Result<Vec<u32>, String> {
                    let Val::List(items) = val else {
                        return Err(format!("{label} must be List of integers"));
                    };
                    items
                        .iter()
                        .map(|x| match x {
                            Val::Num(n)
                                if *n >= 0.0 && n.fract() == 0.0 && *n <= u32::MAX as f64 =>
                            {
                                Ok(*n as u32)
                            }
                            _ => Err(format!("{label} entries must be uint32 integers")),
                        })
                        .collect()
                };
                let words = parse_u32s(&a[0], "spirv words")?;
                let buf_a = as_num_list(&a[1])?;
                let buf_b = as_num_list(&a[2])?;
                let out_len = parse_pos_int(Some(&a[3]), 1, "gpu_run_spirv out_len")?;
                let (push_words, dim_idx) = match a.get(4) {
                    Some(Val::List(_)) => (parse_u32s(&a[4], "push_words")?, 5),
                    _ => (Vec::new(), 4),
                };
                let gx = parse_pos_int(a.get(dim_idx), out_len.div_ceil(64).max(1), "gx")? as u32;
                let gy = parse_pos_int(a.get(dim_idx + 1), 1, "gy")? as u32;
                let gz = parse_pos_int(a.get(dim_idx + 2), 1, "gz")? as u32;
                let out = crate::gpu::gpu_run_spirv(
                    &words,
                    &buf_a,
                    &buf_b,
                    out_len,
                    &push_words,
                    gx,
                    gy,
                    gz,
                )?;
                nums_to_val(out)
            }
            _ => return Ok(None),
        };
        Ok(Some(v))
    }
}
