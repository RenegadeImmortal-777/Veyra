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
    AdamState, TensorOp, adam_states, tensor_backward, tensor_from_value, tensor_id,
    tensor_snapshot, tensor_store, tensors,
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
    pub(crate) fn call_builtin_data_gui(
        &mut self,
        n: &str,
        a: Vec<Val>,
    ) -> Result<Option<Val>, String> {
        let v = match n {
            "table_column" => {
                if a.len() != 2 {
                    return Err("table_column expects rows,name".into());
                }
                let rows = match &a[0] {
                    Val::List(v) => v,
                    _ => return Err("rows must be List".into()),
                };
                let name = match &a[1] {
                    Val::Str(s) => Val::Str(s.clone()),
                    _ => return Err("name must be String".into()),
                };
                let mut out = vec![];
                for row in rows {
                    let pairs = match row {
                        Val::Map(p) => p,
                        _ => return Err("rows must contain Map values".into()),
                    };
                    out.push(map_lookup(pairs, &name).cloned().unwrap_or(Val::Null));
                }
                Val::List(out)
            }
            "table_filter_eq" => {
                if a.len() != 3 {
                    return Err("table_filter_eq expects rows,name,value".into());
                }
                let rows = match &a[0] {
                    Val::List(v) => v,
                    _ => return Err("rows must be List".into()),
                };
                let key = match &a[1] {
                    Val::Str(s) => Val::Str(s.clone()),
                    _ => return Err("name must be String".into()),
                };
                let target = &a[2];
                Val::List(
                    rows.iter()
                        .filter(|row| match row {
                            Val::Map(p) => {
                                map_lookup(p, &key).map(|v| v == target).unwrap_or(false)
                            }
                            _ => false,
                        })
                        .cloned()
                        .collect(),
                )
            }
            "table_describe" => {
                if a.len() != 2 {
                    return Err("table_describe expects rows,name".into());
                }
                let rows = match &a[0] {
                    Val::List(v) => v,
                    _ => return Err("rows must be List".into()),
                };
                let key = match &a[1] {
                    Val::Str(s) => Val::Str(s.clone()),
                    _ => return Err("name must be String".into()),
                };
                let nums: Vec<f64> = rows
                    .iter()
                    .filter_map(|row| match row {
                        Val::Map(p) => map_lookup(p, &key).and_then(|v| match v {
                            Val::Num(n) => Some(n.to_owned()),
                            _ => None,
                        }),
                        _ => None,
                    })
                    .collect();
                if nums.is_empty() {
                    return Err("table_describe found no numeric values".into());
                }
                let count = nums.len() as f64;
                let mean = nums.iter().sum::<f64>() / count;
                let min = nums.iter().fold(f64::INFINITY, |m, x| m.min(*x));
                let max = nums.iter().fold(f64::NEG_INFINITY, |m, x| m.max(*x));
                Val::Map(vec![
                    (Val::Str("count".into()), Val::Num(count)),
                    (Val::Str("mean".into()), Val::Num(mean)),
                    (Val::Str("min".into()), Val::Num(min)),
                    (Val::Str("max".into()), Val::Num(max)),
                ])
            }
            "plot_svg" => {
                if a.len() != 3 {
                    return Err("plot_svg expects path,xs,ys".into());
                }
                let path = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("path must be String".into()),
                };
                let xs = as_num_list(&a[1])?;
                let ys = as_num_list(&a[2])?;
                if xs.len() != ys.len() || xs.len() < 2 {
                    return Err("plot_svg requires equal lists length >=2".into());
                }
                let xmin = xs.iter().fold(f64::INFINITY, |m, x| m.min(*x));
                let xmax = xs.iter().fold(f64::NEG_INFINITY, |m, x| m.max(*x));
                let ymin = ys.iter().fold(f64::INFINITY, |m, x| m.min(*x));
                let ymax = ys.iter().fold(f64::NEG_INFINITY, |m, x| m.max(*x));
                let sx = if xmax == xmin { 1.0 } else { xmax - xmin };
                let sy = if ymax == ymin { 1.0 } else { ymax - ymin };
                let pts = xs
                    .iter()
                    .zip(ys.iter())
                    .map(|(x, y)| {
                        let px = 50.0 + 700.0 * (x - xmin) / sx;
                        let py = 450.0 - 400.0 * (y - ymin) / sy;
                        format!("{px:.2},{py:.2}")
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                let svg = format!(
                    "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"800\" height=\"500\"><rect width=\"100%\" height=\"100%\" fill=\"white\"/><line x1=\"50\" y1=\"450\" x2=\"750\" y2=\"450\" stroke=\"black\"/><line x1=\"50\" y1=\"50\" x2=\"50\" y2=\"450\" stroke=\"black\"/><polyline fill=\"none\" stroke=\"black\" stroke-width=\"2\" points=\"{}\"/></svg>",
                    svg_escape(&pts)
                );
                fs::write(path, svg).map_err(|e| e.to_string())?;
                Val::Null
            }
            "scatter_svg" => {
                if a.len() != 3 {
                    return Err("scatter_svg expects path,xs,ys".into());
                }
                let path = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("path must be String".into()),
                };
                let xs = as_num_list(&a[1])?;
                let ys = as_num_list(&a[2])?;
                if xs.len() != ys.len() || xs.is_empty() {
                    return Err("scatter_svg requires equal non-empty lists".into());
                }
                let xmin = xs.iter().fold(f64::INFINITY, |m, x| m.min(*x));
                let xmax = xs.iter().fold(f64::NEG_INFINITY, |m, x| m.max(*x));
                let ymin = ys.iter().fold(f64::INFINITY, |m, x| m.min(*x));
                let ymax = ys.iter().fold(f64::NEG_INFINITY, |m, x| m.max(*x));
                let sx = if xmax == xmin { 1.0 } else { xmax - xmin };
                let sy = if ymax == ymin { 1.0 } else { ymax - ymin };
                let dots = xs
                    .iter()
                    .zip(ys.iter())
                    .map(|(x, y)| {
                        let px = 50.0 + 700.0 * (x - xmin) / sx;
                        let py = 450.0 - 400.0 * (y - ymin) / sy;
                        format!("<circle cx=\"{px:.2}\" cy=\"{py:.2}\" r=\"4\"/>")
                    })
                    .collect::<Vec<_>>()
                    .join("");
                let svg = format!(
                    "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"800\" height=\"500\"><rect width=\"100%\" height=\"100%\" fill=\"white\"/><line x1=\"50\" y1=\"450\" x2=\"750\" y2=\"450\" stroke=\"black\"/><line x1=\"50\" y1=\"50\" x2=\"50\" y2=\"450\" stroke=\"black\"/>{dots}</svg>"
                );
                fs::write(path, svg).map_err(|e| e.to_string())?;
                Val::Null
            }
            "bar_svg" => {
                if a.len() != 3 {
                    return Err("bar_svg expects path,labels,values".into());
                }
                let path = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("path must be String".into()),
                };
                let labels = match &a[1] {
                    Val::List(v) => v,
                    _ => return Err("labels must be List".into()),
                };
                let vals = as_num_list(&a[2])?;
                if labels.len() != vals.len() || vals.is_empty() {
                    return Err("bar_svg requires equal non-empty lists".into());
                }
                let max = vals.iter().copied().fold(0.0, f64::max).max(1.0);
                let width = 700.0 / vals.len() as f64;
                let mut body = String::new();
                for (i, (label, value)) in labels.iter().zip(vals.iter()).enumerate() {
                    let h = 360.0 * (*value) / max;
                    let x = 55.0 + i as f64 * width;
                    let y = 440.0 - h;
                    body.push_str(&format!("<rect x=\"{x:.2}\" y=\"{y:.2}\" width=\"{:.2}\" height=\"{h:.2}\"/><text x=\"{:.2}\" y=\"470\" font-size=\"12\" text-anchor=\"middle\">{}</text>",(width-10.0).max(1.0),x+(width-10.0).max(1.0)/2.0,svg_escape(&label.show())));
                }
                let svg = format!(
                    "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"800\" height=\"500\"><rect width=\"100%\" height=\"100%\" fill=\"white\"/><line x1=\"50\" y1=\"440\" x2=\"760\" y2=\"440\" stroke=\"black\"/>{body}</svg>"
                );
                fs::write(path, svg).map_err(|e| e.to_string())?;
                Val::Null
            }
            "histogram" => {
                if a.len() != 2 {
                    return Err("histogram expects values,bins".into());
                }
                let vals = as_num_list(&a[0])?;
                let bins = match a[1] {
                    Val::Num(n) => n.max(1.0) as usize,
                    _ => return Err("bins must be Number".into()),
                };
                if vals.is_empty() {
                    return Err("histogram requires values".into());
                }
                let min = vals.iter().copied().fold(f64::INFINITY, f64::min);
                let max = vals.iter().copied().fold(f64::NEG_INFINITY, f64::max);
                let span = (max - min).max(f64::EPSILON);
                let mut counts = vec![0usize; bins];
                for v in vals {
                    let mut idx = (((v - min) / span) * bins as f64).floor() as usize;
                    if idx >= bins {
                        idx = bins - 1;
                    }
                    counts[idx] += 1;
                }
                Val::List(
                    counts
                        .into_iter()
                        .enumerate()
                        .map(|(i, count)| {
                            let lo = min + span * i as f64 / bins as f64;
                            let hi = min + span * (i + 1) as f64 / bins as f64;
                            Val::Map(vec![
                                (Val::Str("low".into()), Val::Num(lo)),
                                (Val::Str("high".into()), Val::Num(hi)),
                                (Val::Str("count".into()), Val::Num(count as f64)),
                            ])
                        })
                        .collect(),
                )
            }
            "logreg_fit" => {
                if a.len() != 4 {
                    return Err("logreg_fit expects X,y,iterations,learning_rate".into());
                }
                let x = as_matrix(&a[0])?;
                let y = as_num_list(&a[1])?;
                let iterations = match a[2] {
                    Val::Num(n) => n.max(1.0) as usize,
                    _ => return Err("iterations must be Number".into()),
                };
                let lr = match a[3] {
                    Val::Num(n) => n,
                    _ => return Err("learning_rate must be Number".into()),
                };
                if x.is_empty() || x.len() != y.len() {
                    return Err("logreg_fit row mismatch".into());
                }
                let dims = x[0].len();
                if dims == 0 || x.iter().any(|r| r.len() != dims) {
                    return Err("invalid feature matrix".into());
                }
                let mut w = vec![0.0; dims];
                let mut b = 0.0;
                for _ in 0..iterations {
                    let mut gw = vec![0.0; dims];
                    let mut gb = 0.0;
                    for (row, target) in x.iter().zip(y.iter()) {
                        let z = b + w
                            .iter()
                            .zip(row.iter())
                            .map(|(wi, xi)| wi * xi)
                            .sum::<f64>();
                        let p = 1.0 / (1.0 + (-z).exp());
                        let e = p - target;
                        for j in 0..dims {
                            gw[j] += e * row[j];
                        }
                        gb += e;
                    }
                    let n = x.len() as f64;
                    for j in 0..dims {
                        w[j] -= lr * gw[j] / n;
                    }
                    b -= lr * gb / n;
                }
                Val::Map(vec![
                    (Val::Str("weights".into()), nums_to_val(w)),
                    (Val::Str("bias".into()), Val::Num(b)),
                ])
            }
            "logreg_predict" => {
                if a.len() != 2 {
                    return Err("logreg_predict expects model,features".into());
                }
                let model = match &a[0] {
                    Val::Map(p) => p,
                    _ => return Err("model must be Map".into()),
                };
                let features = as_num_list(&a[1])?;
                let weights = map_lookup(model, &Val::Str("weights".into()))
                    .ok_or("model missing weights")?;
                let weights = as_num_list(weights)?;
                let bias = map_lookup(model, &Val::Str("bias".into()))
                    .and_then(|v| if let Val::Num(n) = v { Some(*n) } else { None })
                    .ok_or("model missing bias")?;
                if weights.len() != features.len() {
                    return Err("logreg_predict dimension mismatch".into());
                }
                let z = bias
                    + weights
                        .iter()
                        .zip(features.iter())
                        .map(|(w, x)| w * x)
                        .sum::<f64>();
                Val::Num(1.0 / (1.0 + (-z).exp()))
            }
            "table_group_count" => {
                if a.len() != 2 {
                    return Err("table_group_count expects rows,name".into());
                }
                let rows = match &a[0] {
                    Val::List(v) => v,
                    _ => return Err("rows must be List".into()),
                };
                let key = match &a[1] {
                    Val::Str(s) => Val::Str(s.clone()),
                    _ => return Err("name must be String".into()),
                };
                let mut groups: Vec<(Val, usize)> = vec![];
                for row in rows {
                    let pairs = match row {
                        Val::Map(p) => p,
                        _ => return Err("rows must contain Maps".into()),
                    };
                    let value = map_lookup(pairs, &key).cloned().unwrap_or(Val::Null);
                    if let Some((_, n)) = groups.iter_mut().find(|(v, _)| *v == value) {
                        *n += 1
                    } else {
                        groups.push((value, 1))
                    }
                }
                Val::List(
                    groups
                        .into_iter()
                        .map(|(value, count)| {
                            Val::Map(vec![
                                (Val::Str("value".into()), value),
                                (Val::Str("count".into()), Val::Num(count as f64)),
                            ])
                        })
                        .collect(),
                )
            }
            "table_group_sum" => {
                if a.len() != 3 {
                    return Err("table_group_sum expects rows,group,value".into());
                }
                let rows = match &a[0] {
                    Val::List(v) => v,
                    _ => return Err("rows must be List".into()),
                };
                let gk = match &a[1] {
                    Val::Str(s) => Val::Str(s.clone()),
                    _ => return Err("group must be String".into()),
                };
                let vk = match &a[2] {
                    Val::Str(s) => Val::Str(s.clone()),
                    _ => return Err("value must be String".into()),
                };
                let mut groups: Vec<(Val, f64)> = vec![];
                for row in rows {
                    let p = match row {
                        Val::Map(p) => p,
                        _ => return Err("rows must contain Maps".into()),
                    };
                    let group = map_lookup(p, &gk).cloned().unwrap_or(Val::Null);
                    let value = map_lookup(p, &vk)
                        .and_then(|v| if let Val::Num(n) = v { Some(*n) } else { None })
                        .ok_or("table_group_sum requires numeric value column")?;
                    if let Some((_, sum)) = groups.iter_mut().find(|(g, _)| *g == group) {
                        *sum += value
                    } else {
                        groups.push((group, value));
                    }
                }
                Val::List(
                    groups
                        .into_iter()
                        .map(|(group, sum)| {
                            Val::Map(vec![
                                (Val::Str("value".into()), group),
                                (Val::Str("sum".into()), Val::Num(sum)),
                            ])
                        })
                        .collect(),
                )
            }
            "table_group_mean" => {
                if a.len() != 3 {
                    return Err("table_group_mean expects rows,group,value".into());
                }
                let rows = match &a[0] {
                    Val::List(v) => v,
                    _ => return Err("rows must be List".into()),
                };
                let gk = match &a[1] {
                    Val::Str(s) => Val::Str(s.clone()),
                    _ => return Err("group must be String".into()),
                };
                let vk = match &a[2] {
                    Val::Str(s) => Val::Str(s.clone()),
                    _ => return Err("value must be String".into()),
                };
                let mut groups: Vec<(Val, f64, usize)> = vec![];
                for row in rows {
                    let p = match row {
                        Val::Map(p) => p,
                        _ => return Err("rows must contain Maps".into()),
                    };
                    let group = map_lookup(p, &gk).cloned().unwrap_or(Val::Null);
                    let value = map_lookup(p, &vk)
                        .and_then(|v| if let Val::Num(n) = v { Some(*n) } else { None })
                        .ok_or("table_group_mean requires numeric value column")?;
                    if let Some((_, sum, count)) = groups.iter_mut().find(|(g, _, _)| *g == group) {
                        *sum += value;
                        *count += 1
                    } else {
                        groups.push((group, value, 1));
                    }
                }
                Val::List(
                    groups
                        .into_iter()
                        .map(|(group, sum, count)| {
                            Val::Map(vec![
                                (Val::Str("value".into()), group),
                                (Val::Str("mean".into()), Val::Num(sum / count as f64)),
                                (Val::Str("count".into()), Val::Num(count as f64)),
                            ])
                        })
                        .collect(),
                )
            }
            "table_sort" => {
                if a.len() != 2 {
                    return Err("table_sort expects rows,name".into());
                }
                let mut rows = match &a[0] {
                    Val::List(v) => v.clone(),
                    _ => return Err("rows must be List".into()),
                };
                let key = match &a[1] {
                    Val::Str(s) => Val::Str(s.clone()),
                    _ => return Err("name must be String".into()),
                };
                rows.sort_by(|ra, rb| {
                    let av = if let Val::Map(p) = ra {
                        map_lookup(p, &key)
                    } else {
                        None
                    };
                    let bv = if let Val::Map(p) = rb {
                        map_lookup(p, &key)
                    } else {
                        None
                    };
                    match (av, bv) {
                        (Some(Val::Num(a)), Some(Val::Num(b))) => {
                            a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)
                        }
                        (Some(Val::Str(a)), Some(Val::Str(b))) => a.cmp(b),
                        _ => std::cmp::Ordering::Equal,
                    }
                });
                Val::List(rows)
            }
            "table_select" => {
                if a.len() != 2 {
                    return Err("table_select expects rows,names".into());
                }
                let rows = match &a[0] {
                    Val::List(v) => v,
                    _ => return Err("rows must be List".into()),
                };
                let names = match &a[1] {
                    Val::List(v) => v,
                    _ => return Err("names must be List".into()),
                };
                let keys: Result<Vec<String>, String> = names
                    .iter()
                    .map(|v| match v {
                        Val::Str(s) => Ok(s.clone()),
                        _ => Err("column names must be Strings".into()),
                    })
                    .collect();
                let keys = keys?;
                let mut out = vec![];
                for row in rows {
                    let pairs = match row {
                        Val::Map(p) => p,
                        _ => return Err("rows must contain Maps".into()),
                    };
                    let mut selected = vec![];
                    for name in &keys {
                        let k = Val::Str(name.clone());
                        selected.push((
                            k.clone(),
                            map_lookup(pairs, &k).cloned().unwrap_or(Val::Null),
                        ));
                    }
                    out.push(Val::Map(selected));
                }
                Val::List(out)
            }
            "table_inner_join" => {
                if a.len() != 4 {
                    return Err("table_inner_join expects left,right,left_key,right_key".into());
                }
                let left = match &a[0] {
                    Val::List(v) => v,
                    _ => return Err("left must be List".into()),
                };
                let right = match &a[1] {
                    Val::List(v) => v,
                    _ => return Err("right must be List".into()),
                };
                let lk = match &a[2] {
                    Val::Str(s) => Val::Str(s.clone()),
                    _ => return Err("left_key must be String".into()),
                };
                let rk = match &a[3] {
                    Val::Str(s) => Val::Str(s.clone()),
                    _ => return Err("right_key must be String".into()),
                };
                let mut out = vec![];
                for lrow in left {
                    let lp = match lrow {
                        Val::Map(p) => p,
                        _ => return Err("left rows must be Maps".into()),
                    };
                    let lv = map_lookup(lp, &lk);
                    for rrow in right {
                        let rp = match rrow {
                            Val::Map(p) => p,
                            _ => return Err("right rows must be Maps".into()),
                        };
                        if lv.is_some() && lv == map_lookup(rp, &rk) {
                            let mut merged = lp.clone();
                            for (k, v) in rp {
                                let key = if merged.iter().any(|(existing, _)| existing == k) {
                                    Val::Str(format!("right.{}", k.show()))
                                } else {
                                    k.clone()
                                };
                                merged.push((key, v.clone()));
                            }
                            out.push(Val::Map(merged));
                        }
                    }
                }
                Val::List(out)
            }
            "table_head" => {
                if a.len() != 2 {
                    return Err("table_head expects rows,count".into());
                }
                let rows = match &a[0] {
                    Val::List(v) => v,
                    _ => return Err("rows must be List".into()),
                };
                let n = match a[1] {
                    Val::Num(n) => n.max(0.0) as usize,
                    _ => return Err("count must be Number".into()),
                };
                Val::List(rows.iter().take(n).cloned().collect())
            }
            "table_tail" => {
                if a.len() != 2 {
                    return Err("table_tail expects rows,count".into());
                }
                let rows = match &a[0] {
                    Val::List(v) => v,
                    _ => return Err("rows must be List".into()),
                };
                let n = match a[1] {
                    Val::Num(n) => n.max(0.0) as usize,
                    _ => return Err("count must be Number".into()),
                };
                let start = rows.len().saturating_sub(n);
                Val::List(rows[start..].to_vec())
            }
            "table_drop_nulls" => {
                if a.len() != 2 {
                    return Err("table_drop_nulls expects rows,column".into());
                }
                let rows = match &a[0] {
                    Val::List(v) => v,
                    _ => return Err("rows must be List".into()),
                };
                let key = match &a[1] {
                    Val::Str(s) => Val::Str(s.clone()),
                    _ => return Err("column must be String".into()),
                };
                Val::List(
                    rows.iter()
                        .filter(|row| match row {
                            Val::Map(p) => map_lookup(p, &key)
                                .map(|v| !matches!(v, Val::Null))
                                .unwrap_or(false),
                            _ => false,
                        })
                        .cloned()
                        .collect(),
                )
            }
            "table_fill_null" => {
                if a.len() != 3 {
                    return Err("table_fill_null expects rows,column,value".into());
                }
                let rows = match &a[0] {
                    Val::List(v) => v,
                    _ => return Err("rows must be List".into()),
                };
                let key = match &a[1] {
                    Val::Str(s) => Val::Str(s.clone()),
                    _ => return Err("column must be String".into()),
                };
                let fill = a[2].clone();
                let mut out = vec![];
                for row in rows {
                    let mut pairs = match row {
                        Val::Map(p) => p.clone(),
                        _ => return Err("rows must contain Maps".into()),
                    };
                    if let Some((_, v)) = pairs.iter_mut().find(|(k, _)| *k == key) {
                        if matches!(v, Val::Null) {
                            *v = fill.clone();
                        }
                    } else {
                        pairs.push((key.clone(), fill.clone()));
                    }
                    out.push(Val::Map(pairs));
                }
                Val::List(out)
            }
            "table_left_join" => {
                if a.len() != 4 {
                    return Err("table_left_join expects left,right,left_key,right_key".into());
                }
                let left = match &a[0] {
                    Val::List(v) => v,
                    _ => return Err("left must be List".into()),
                };
                let right = match &a[1] {
                    Val::List(v) => v,
                    _ => return Err("right must be List".into()),
                };
                let lk = match &a[2] {
                    Val::Str(s) => Val::Str(s.clone()),
                    _ => return Err("left_key must be String".into()),
                };
                let rk = match &a[3] {
                    Val::Str(s) => Val::Str(s.clone()),
                    _ => return Err("right_key must be String".into()),
                };
                let mut out = vec![];
                for lrow in left {
                    let lp = match lrow {
                        Val::Map(p) => p,
                        _ => return Err("left rows must be Maps".into()),
                    };
                    let lv = map_lookup(lp, &lk);
                    let mut matched = false;
                    for rrow in right {
                        let rp = match rrow {
                            Val::Map(p) => p,
                            _ => return Err("right rows must be Maps".into()),
                        };
                        if lv.is_some() && lv == map_lookup(rp, &rk) {
                            matched = true;
                            let mut merged = lp.clone();
                            for (k, v) in rp {
                                let key = if merged.iter().any(|(existing, _)| existing == k) {
                                    Val::Str(format!("right.{}", k.show()))
                                } else {
                                    k.clone()
                                };
                                merged.push((key, v.clone()));
                            }
                            out.push(Val::Map(merged));
                        }
                    }
                    if !matched {
                        out.push(Val::Map(lp.clone()));
                    }
                }
                Val::List(out)
            }
            "csv_read" => {
                if a.len() != 1 {
                    return Err("csv_read expects path".into());
                }
                let p = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("path must be String".into()),
                };
                let mut rdr = csv::Reader::from_path(p).map_err(|e| e.to_string())?;
                let headers = rdr
                    .headers()
                    .map_err(|e| e.to_string())?
                    .iter()
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>();
                let mut rows = vec![];
                for rec in rdr.records() {
                    let rec = rec.map_err(|e| e.to_string())?;
                    let pairs = headers
                        .iter()
                        .zip(rec.iter())
                        .map(|(h, v)| {
                            let val = v
                                .parse::<f64>()
                                .map(Val::Num)
                                .unwrap_or_else(|_| Val::Str(v.to_string()));
                            (Val::Str(h.clone()), val)
                        })
                        .collect();
                    rows.push(Val::Map(pairs));
                }
                Val::List(rows)
            }
            "csv_write" => {
                if a.len() != 2 {
                    return Err("csv_write expects path,rows".into());
                }
                let p = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("path must be String".into()),
                };
                let rows = match &a[1] {
                    Val::List(v) => v,
                    _ => return Err("rows must be List".into()),
                };
                let mut w = csv::Writer::from_path(p).map_err(|e| e.to_string())?;
                if let Some(Val::Map(first)) = rows.first() {
                    let headers = first.iter().map(|(k, _)| k.show()).collect::<Vec<_>>();
                    w.write_record(&headers).map_err(|e| e.to_string())?;
                    for row in rows {
                        let map = match row {
                            Val::Map(p) => p,
                            _ => return Err("all csv rows must be Map".into()),
                        };
                        let vals = headers
                            .iter()
                            .map(|h| {
                                map.iter()
                                    .find(|(k, _)| k.show() == *h)
                                    .map(|(_, v)| v.show())
                                    .unwrap_or_default()
                            })
                            .collect::<Vec<_>>();
                        w.write_record(vals).map_err(|e| e.to_string())?;
                    }
                }
                w.flush().map_err(|e| e.to_string())?;
                Val::Null
            }
            #[cfg(feature = "native-services")]
            "gui_message" => {
                if a.len() != 2 {
                    return Err("gui_message expects title,message".into());
                }
                let title = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("title must be String".into()),
                };
                let msg = match &a[1] {
                    Val::Str(s) => s,
                    _ => return Err("message must be String".into()),
                };
                tinyfiledialogs::message_box_ok(title, msg, tinyfiledialogs::MessageBoxIcon::Info);
                Val::Null
            }
            #[cfg(feature = "native-services")]
            "gui_input" => {
                if a.len() != 2 {
                    return Err("gui_input expects title,message".into());
                }
                let title = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("title must be String".into()),
                };
                let msg = match &a[1] {
                    Val::Str(s) => s,
                    _ => return Err("message must be String".into()),
                };
                tinyfiledialogs::input_box(title, msg, "")
                    .map(Val::Str)
                    .unwrap_or(Val::Null)
            }
            #[cfg(feature = "native-services")]
            "gui_open_file" => {
                if a.len() != 1 {
                    return Err("gui_open_file expects title".into());
                }
                let title = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("title must be String".into()),
                };
                tinyfiledialogs::open_file_dialog(title, "", None)
                    .map(Val::Str)
                    .unwrap_or(Val::Null)
            }
            #[cfg(feature = "native-services")]
            "gui_save_file" => {
                if a.len() != 1 {
                    return Err("gui_save_file expects title".into());
                }
                let title = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("title must be String".into()),
                };
                tinyfiledialogs::save_file_dialog(title, "")
                    .map(Val::Str)
                    .unwrap_or(Val::Null)
            }
            #[cfg(feature = "native-services")]
            "gui_confirm" => {
                if a.len() != 2 {
                    return Err("gui_confirm expects title,message".into());
                }
                let title = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("title must be String".into()),
                };
                let msg = match &a[1] {
                    Val::Str(s) => s,
                    _ => return Err("message must be String".into()),
                };
                Val::Bool(matches!(
                    tinyfiledialogs::message_box_yes_no(
                        title,
                        msg,
                        tinyfiledialogs::MessageBoxIcon::Question,
                        tinyfiledialogs::YesNo::Yes
                    ),
                    tinyfiledialogs::YesNo::Yes
                ))
            }
            #[cfg(feature = "native-services")]
            "gui_password" => {
                if a.len() != 2 {
                    return Err("gui_password expects title,message".into());
                }
                let title = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("title must be String".into()),
                };
                let msg = match &a[1] {
                    Val::Str(s) => s,
                    _ => return Err("message must be String".into()),
                };
                tinyfiledialogs::password_box(title, msg)
                    .map(Val::Str)
                    .unwrap_or(Val::Null)
            }
            #[cfg(feature = "native-services")]
            "gui_folder" => {
                if a.len() != 1 {
                    return Err("gui_folder expects title".into());
                }
                let title = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("title must be String".into()),
                };
                tinyfiledialogs::select_folder_dialog(title, "")
                    .map(Val::Str)
                    .unwrap_or(Val::Null)
            }
            #[cfg(feature = "native-services")]
            "gui_color" => {
                if a.len() != 1 {
                    return Err("gui_color expects title".into());
                }
                let title = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("title must be String".into()),
                };
                match tinyfiledialogs::color_chooser_dialog(
                    title,
                    tinyfiledialogs::DefaultColorValue::Hex("#ffffff"),
                ) {
                    Some((hex, _)) => Val::Str(hex),
                    None => Val::Null,
                }
            }
            #[cfg(feature = "native-services")]
            "gui_form" => {
                if a.len() != 2 {
                    return Err("gui_form expects title,fields".into());
                }
                let title = match &a[0] {
                    Val::Str(s) => s,
                    _ => return Err("title must be String".into()),
                };
                let fields = match &a[1] {
                    Val::List(v) => v,
                    _ => return Err("fields must be List".into()),
                };
                let mut out = vec![];
                for field in fields {
                    let label = match field {
                        Val::Str(s) => s,
                        _ => return Err("field names must be String".into()),
                    };
                    match tinyfiledialogs::input_box(title, label, "") {
                        Some(value) => out.push((Val::Str(label.clone()), Val::Str(value))),
                        None => return Ok(Some(Val::Null)),
                    }
                }
                Val::Map(out)
            }
            "gui_app" => {
                crate::runtime::require_cap("io")?;
                if a.len() != 2 {
                    return Err("gui_app expects (layout_map, on_event_fn)".into());
                }
                return crate::gui_native::run_gui_app(self, &a[0], &a[1]).map(Some);
            }
            _ => return Ok(None),
        };
        Ok(Some(v))
    }
}
