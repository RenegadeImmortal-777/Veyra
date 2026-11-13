// Tensor runtime: handles, shape/autograd topology and the reverse-mode
// backward pass behind the `tensor_*` builtins.
//
// Extracted from `main.rs` without behavior change; see docs/ENGINE-SPLIT.md.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::{
    Mutex, OnceLock,
    atomic::{AtomicU64, Ordering},
};

use crate::value::{Val, as_num_list};

pub(crate) static TENSOR_IDS: AtomicU64 = AtomicU64::new(1);
pub(crate) static TENSORS: OnceLock<Mutex<HashMap<u64, TensorData>>> = OnceLock::new();
pub(crate) static ADAM_STATES: OnceLock<Mutex<HashMap<u64, AdamState>>> = OnceLock::new();

thread_local! {
    static PENDING_TAPE_IDS: RefCell<HashSet<u64>> = RefCell::new(HashSet::new());
}

#[derive(Clone, Debug)]
pub(crate) enum TensorOp {
    Leaf,
    Add(u64, u64),
    Sub(u64, u64),
    Mul(u64, u64),
    Div(u64, u64),
    Neg(u64),
    Pow(u64, f64),
    SoftmaxRows(u64, usize, usize),
    Relu(u64),
    Sigmoid(u64),
    Tanh(u64),
    Exp(u64),
    Log(u64),
    Reshape(u64),
    Transpose(u64, usize, usize),
    Sum(u64),
    Mean(u64),
    MatMul(u64, u64, usize, usize, usize),
    Scale(u64, f64),
    /// Mean cross-entropy over rows: (logits, target class per row, rows, cols).
    CrossEntropy(u64, Vec<usize>, usize, usize),
    /// Normalization over the last dimension: (input, rows, cols, epsilon).
    LayerNorm(u64, usize, usize, f64),
    /// 2-D convolution (input NCHW, weight OCKhKw, dims).
    Conv2d(u64, u64, Conv2dDims),
    /// Slice columns [col_start .. col_start + col_len] from 2D [rows, total_cols].
    SliceCols(u64, usize, usize, usize, usize),
    /// Concatenate 2D tensors horizontally: (parent_ids, rows, col_lens).
    ConcatCols(Vec<u64>, usize, Vec<usize>),
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Conv2dDims {
    pub(crate) n: usize,
    pub(crate) c: usize,
    pub(crate) h: usize,
    pub(crate) w: usize,
    pub(crate) o: usize,
    pub(crate) kh: usize,
    pub(crate) kw: usize,
    pub(crate) stride: usize,
    pub(crate) oh: usize,
    pub(crate) ow: usize,
}

impl Conv2dDims {
    pub(crate) fn in_idx(&self, n: usize, c: usize, y: usize, x: usize) -> usize {
        ((n * self.c + c) * self.h + y) * self.w + x
    }
    pub(crate) fn w_idx(&self, o: usize, c: usize, i: usize, j: usize) -> usize {
        ((o * self.c + c) * self.kh + i) * self.kw + j
    }
    pub(crate) fn out_idx(&self, n: usize, o: usize, y: usize, x: usize) -> usize {
        ((n * self.o + o) * self.oh + y) * self.ow + x
    }
}

pub(crate) fn conv2d_forward(input: &[f64], weight: &[f64], d: &Conv2dDims) -> Vec<f64> {
    let mut out = vec![0.0; d.n * d.o * d.oh * d.ow];
    for n in 0..d.n {
        for o in 0..d.o {
            for y in 0..d.oh {
                for x in 0..d.ow {
                    let mut acc = 0.0;
                    for c in 0..d.c {
                        for i in 0..d.kh {
                            for j in 0..d.kw {
                                acc += input[d.in_idx(n, c, y * d.stride + i, x * d.stride + j)]
                                    * weight[d.w_idx(o, c, i, j)];
                            }
                        }
                    }
                    out[d.out_idx(n, o, y, x)] = acc;
                }
            }
        }
    }
    out
}

/// Row-wise numerically stable softmax over a `rows x cols` buffer.
pub(crate) fn softmax_rows(data: &[f64], rows: usize, cols: usize) -> Vec<f64> {
    let mut out = vec![0.0; rows * cols];
    for r in 0..rows {
        let row = &data[r * cols..(r + 1) * cols];
        let max = row.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let mut denom = 0.0;
        for c in 0..cols {
            let e = (row[c] - max).exp();
            out[r * cols + c] = e;
            denom += e;
        }
        for c in 0..cols {
            out[r * cols + c] /= denom;
        }
    }
    out
}

/// Per-row mean and inverse standard deviation for layer normalization.
pub(crate) fn layer_norm_stats(
    data: &[f64],
    rows: usize,
    cols: usize,
    eps: f64,
) -> Vec<(f64, f64)> {
    (0..rows)
        .map(|r| {
            let row = &data[r * cols..(r + 1) * cols];
            let mean = row.iter().sum::<f64>() / cols as f64;
            let var = row.iter().map(|x| (x - mean) * (x - mean)).sum::<f64>() / cols as f64;
            (mean, 1.0 / (var + eps).sqrt())
        })
        .collect()
}

#[derive(Clone, Debug)]
pub(crate) struct TensorData {
    pub(crate) data: Vec<f64>,
    pub(crate) shape: Vec<usize>,
    pub(crate) grad: Vec<f64>,
    pub(crate) requires_grad: bool,
    pub(crate) op: TensorOp,
}

#[derive(Clone, Debug)]
pub(crate) struct AdamState {
    pub(crate) m: Vec<f64>,
    pub(crate) v: Vec<f64>,
    pub(crate) step: u64,
}

pub(crate) fn tensors() -> &'static Mutex<HashMap<u64, TensorData>> {
    TENSORS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn adam_states() -> &'static Mutex<HashMap<u64, AdamState>> {
    ADAM_STATES.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn tensor_handle(id: u64, shape: &[usize]) -> Val {
    Val::Map(vec![
        (Val::Str("__tensor_id".into()), Val::Num(id as f64)),
        (
            Val::Str("shape".into()),
            Val::List(shape.iter().map(|n| Val::Num(*n as f64)).collect()),
        ),
    ])
}

pub(crate) fn tensor_id(value: &Val) -> Result<u64, String> {
    let pairs = match value {
        Val::Map(p) => p,
        _ => return Err("expected Tensor handle".into()),
    };
    for (k, v) in pairs {
        if *k == Val::Str("__tensor_id".into()) {
            if let Val::Num(n) = v {
                return Ok(*n as u64);
            }
        }
    }
    Err("invalid Tensor handle".into())
}

fn extract_nd_tensor(value: &Val) -> Result<(Vec<f64>, Vec<usize>), String> {
    match value {
        Val::Num(n) => Ok((vec![*n], vec![])),
        Val::List(items) => {
            if items.is_empty() {
                return Ok((vec![], vec![0]));
            }
            if items.iter().all(|x| matches!(x, Val::List(_))) {
                let mut flat = Vec::new();
                let mut child_shape: Option<Vec<usize>> = None;
                for item in items {
                    let (data, shape) = extract_nd_tensor(item)?;
                    match &child_shape {
                        None => child_shape = Some(shape),
                        Some(expected) if expected != &shape => {
                            return Err("tensor nested lists must have equal inner shapes".into());
                        }
                        _ => {}
                    }
                    flat.extend(data);
                }
                let mut full_shape =
                    Vec::with_capacity(1 + child_shape.as_ref().map_or(0, |s| s.len()));
                full_shape.push(items.len());
                if let Some(sub) = child_shape {
                    full_shape.extend(sub);
                }
                Ok((flat, full_shape))
            } else if items.iter().any(|x| matches!(x, Val::List(_))) {
                Err("tensor nested lists must have consistent rank".into())
            } else {
                let xs = as_num_list(value)?;
                let n = xs.len();
                Ok((xs, vec![n]))
            }
        }
        _ => Err("tensor expects Number or numeric List".into()),
    }
}

pub(crate) fn tensor_from_value(value: &Val, requires_grad: bool) -> Result<Val, String> {
    let (data, shape) = extract_nd_tensor(value)?;
    let id = TENSOR_IDS.fetch_add(1, Ordering::Relaxed);
    tensors()
        .lock()
        .map_err(|_| "tensor registry poisoned")?
        .insert(
            id,
            TensorData {
                grad: vec![0.0; data.len()],
                data,
                shape: shape.clone(),
                requires_grad,
                op: TensorOp::Leaf,
            },
        );
    Ok(tensor_handle(id, &shape))
}

pub(crate) fn tensor_snapshot(id: u64) -> Result<TensorData, String> {
    tensors()
        .lock()
        .map_err(|_| "tensor registry poisoned")?
        .get(&id)
        .cloned()
        .ok_or(format!("unknown tensor {id}"))
}

pub(crate) fn tensor_store(
    data: Vec<f64>,
    shape: Vec<usize>,
    requires_grad: bool,
    op: TensorOp,
) -> Result<Val, String> {
    let id = TENSOR_IDS.fetch_add(1, Ordering::Relaxed);
    tensors()
        .lock()
        .map_err(|_| "tensor registry poisoned")?
        .insert(
            id,
            TensorData {
                grad: vec![0.0; data.len()],
                data,
                shape: shape.clone(),
                requires_grad,
                op,
            },
        );
    Ok(tensor_handle(id, &shape))
}

pub(crate) fn broadcast_shape(a: &[usize], b: &[usize]) -> Result<Vec<usize>, String> {
    if a == b {
        return Ok(a.to_vec());
    }
    let rank = a.len().max(b.len());
    let mut out = vec![1usize; rank];
    let oa = rank - a.len();
    let ob = rank - b.len();
    for d in 0..rank {
        let da = if d >= oa { a[d - oa] } else { 1 };
        let db = if d >= ob { b[d - ob] } else { 1 };
        if da == db {
            out[d] = da;
        } else if da == 1 {
            out[d] = db;
        } else if db == 1 {
            out[d] = da;
        } else {
            return Err("tensor shapes are not broadcast-compatible".into());
        }
    }
    Ok(out)
}

pub(crate) fn broadcast_index(flat_out: usize, out_shape: &[usize], in_shape: &[usize]) -> usize {
    if out_shape == in_shape {
        return flat_out;
    }
    if in_shape.is_empty() {
        return 0;
    }
    let mut rem = flat_out;
    let mut in_flat = 0usize;
    let mut in_stride = 1usize;
    let offset = out_shape.len().saturating_sub(in_shape.len());
    for d in (0..out_shape.len()).rev() {
        let dim = out_shape[d].max(1);
        let coord = rem % dim;
        rem /= dim;
        if d >= offset {
            let in_d = d - offset;
            let in_dim = in_shape[in_d].max(1);
            let in_coord = if in_dim == 1 { 0 } else { coord };
            in_flat += in_coord * in_stride;
            in_stride *= in_dim;
        }
    }
    in_flat
}

pub(crate) fn tensor_topology(
    id: u64,
    seen: &mut HashSet<u64>,
    order: &mut Vec<u64>,
) -> Result<(), String> {
    if !seen.insert(id) {
        return Ok(());
    }
    let t = tensor_snapshot(id)?;
    let parents: Vec<u64> = match t.op {
        TensorOp::Leaf => vec![],
        TensorOp::Add(a, b) | TensorOp::Sub(a, b) | TensorOp::Mul(a, b) | TensorOp::Div(a, b) => {
            vec![a, b]
        }
        TensorOp::Neg(a)
        | TensorOp::Pow(a, _)
        | TensorOp::SoftmaxRows(a, _, _)
        | TensorOp::Relu(a)
        | TensorOp::Sigmoid(a)
        | TensorOp::Tanh(a)
        | TensorOp::Exp(a)
        | TensorOp::Log(a)
        | TensorOp::Reshape(a)
        | TensorOp::Transpose(a, _, _)
        | TensorOp::Sum(a)
        | TensorOp::Mean(a)
        | TensorOp::Scale(a, _)
        | TensorOp::CrossEntropy(a, _, _, _)
        | TensorOp::LayerNorm(a, _, _, _)
        | TensorOp::SliceCols(a, _, _, _, _) => vec![a],
        TensorOp::MatMul(a, b, _, _, _) | TensorOp::Conv2d(a, b, _) => vec![a, b],
        TensorOp::ConcatCols(parents, _, _) => parents,
    };
    for p in parents {
        tensor_topology(p, seen, order)?;
    }
    order.push(id);
    Ok(())
}

pub(crate) fn tensor_backward(root: u64) -> Result<(), String> {
    let root_tensor = tensor_snapshot(root)?;
    if root_tensor.data.len() != 1 {
        return Err("backward expects a scalar tensor; use tensor_sum or tensor_mean first".into());
    }
    let mut order = vec![];
    tensor_topology(root, &mut HashSet::new(), &mut order)?;
    {
        let mut store = tensors().lock().map_err(|_| "tensor registry poisoned")?;
        for id in &order {
            if let Some(tensor) = store.get_mut(id) {
                tensor.grad.fill(0.0);
            }
        }
        let t = store.get_mut(&root).ok_or("unknown tensor")?;
        t.grad = vec![1.0];
    }
    for id in order.iter().copied().rev() {
        let current = tensor_snapshot(id)?;
        let grad = current.grad.clone();
        match current.op {
            TensorOp::Leaf => {}
            TensorOp::Add(a, b) => {
                let mut store = tensors().lock().map_err(|_| "tensor registry poisoned")?;
                for parent in [a, b] {
                    if let Some(t) = store.get_mut(&parent) {
                        if t.requires_grad {
                            let p_shape = t.shape.clone();
                            for (i, g) in grad.iter().enumerate() {
                                let pi = broadcast_index(i, &current.shape, &p_shape);
                                t.grad[pi] += *g;
                            }
                        }
                    }
                }
            }
            TensorOp::Sub(a, b) => {
                let mut store = tensors().lock().map_err(|_| "tensor registry poisoned")?;
                if let Some(t) = store.get_mut(&a) {
                    if t.requires_grad {
                        let p_shape = t.shape.clone();
                        for (i, g) in grad.iter().enumerate() {
                            let pi = broadcast_index(i, &current.shape, &p_shape);
                            t.grad[pi] += *g;
                        }
                    }
                }
                if let Some(t) = store.get_mut(&b) {
                    if t.requires_grad {
                        let p_shape = t.shape.clone();
                        for (i, g) in grad.iter().enumerate() {
                            let pi = broadcast_index(i, &current.shape, &p_shape);
                            t.grad[pi] -= *g;
                        }
                    }
                }
            }
            TensorOp::Mul(a, b) => {
                let left = tensor_snapshot(a)?;
                let right = tensor_snapshot(b)?;
                let mut store = tensors().lock().map_err(|_| "tensor registry poisoned")?;
                if let Some(t) = store.get_mut(&a) {
                    if t.requires_grad {
                        for (i, g) in grad.iter().enumerate() {
                            let ia = broadcast_index(i, &current.shape, &left.shape);
                            let ib = broadcast_index(i, &current.shape, &right.shape);
                            t.grad[ia] += *g * right.data[ib];
                        }
                    }
                }
                if let Some(t) = store.get_mut(&b) {
                    if t.requires_grad {
                        for (i, g) in grad.iter().enumerate() {
                            let ia = broadcast_index(i, &current.shape, &left.shape);
                            let ib = broadcast_index(i, &current.shape, &right.shape);
                            t.grad[ib] += *g * left.data[ia];
                        }
                    }
                }
            }
            TensorOp::Div(a, b) => {
                let left = tensor_snapshot(a)?;
                let right = tensor_snapshot(b)?;
                let mut store = tensors().lock().map_err(|_| "tensor registry poisoned")?;
                if let Some(t) = store.get_mut(&a) {
                    if t.requires_grad {
                        for (i, g) in grad.iter().enumerate() {
                            let ia = broadcast_index(i, &current.shape, &left.shape);
                            let ib = broadcast_index(i, &current.shape, &right.shape);
                            t.grad[ia] += *g / right.data[ib];
                        }
                    }
                }
                if let Some(t) = store.get_mut(&b) {
                    if t.requires_grad {
                        for (i, g) in grad.iter().enumerate() {
                            let ia = broadcast_index(i, &current.shape, &left.shape);
                            let ib = broadcast_index(i, &current.shape, &right.shape);
                            let denom = right.data[ib] * right.data[ib];
                            t.grad[ib] -= *g * left.data[ia] / denom;
                        }
                    }
                }
            }
            TensorOp::Neg(a) => {
                let mut store = tensors().lock().map_err(|_| "tensor registry poisoned")?;
                if let Some(t) = store.get_mut(&a) {
                    if t.requires_grad {
                        for (dst, src) in t.grad.iter_mut().zip(grad.iter()) {
                            *dst -= *src;
                        }
                    }
                }
            }
            TensorOp::Pow(a, exponent) => {
                let parent = tensor_snapshot(a)?;
                let mut store = tensors().lock().map_err(|_| "tensor registry poisoned")?;
                if let Some(t) = store.get_mut(&a) {
                    if t.requires_grad {
                        for i in 0..t.grad.len() {
                            t.grad[i] += grad[i] * exponent * parent.data[i].powf(exponent - 1.0);
                        }
                    }
                }
            }
            TensorOp::SoftmaxRows(a, rows, cols) => {
                let mut incoming = vec![0.0; rows * cols];
                for r in 0..rows {
                    let mut dot = 0.0;
                    for c in 0..cols {
                        let idx = r * cols + c;
                        dot += grad[idx] * current.data[idx];
                    }
                    for c in 0..cols {
                        let idx = r * cols + c;
                        incoming[idx] = current.data[idx] * (grad[idx] - dot);
                    }
                }
                let mut store = tensors().lock().map_err(|_| "tensor registry poisoned")?;
                if let Some(t) = store.get_mut(&a) {
                    if t.requires_grad {
                        for (dst, src) in t.grad.iter_mut().zip(incoming) {
                            *dst += src;
                        }
                    }
                }
            }
            TensorOp::Relu(a) => {
                let parent = tensor_snapshot(a)?;
                let mut store = tensors().lock().map_err(|_| "tensor registry poisoned")?;
                if let Some(t) = store.get_mut(&a) {
                    if t.requires_grad {
                        for i in 0..t.grad.len() {
                            t.grad[i] += if parent.data[i] > 0.0 { grad[i] } else { 0.0 };
                        }
                    }
                }
            }
            TensorOp::Sigmoid(a) => {
                let mut store = tensors().lock().map_err(|_| "tensor registry poisoned")?;
                if let Some(t) = store.get_mut(&a) {
                    if t.requires_grad {
                        for i in 0..t.grad.len() {
                            let y = current.data[i];
                            t.grad[i] += grad[i] * y * (1.0 - y);
                        }
                    }
                }
            }
            TensorOp::Tanh(a) => {
                let mut store = tensors().lock().map_err(|_| "tensor registry poisoned")?;
                if let Some(t) = store.get_mut(&a) {
                    if t.requires_grad {
                        for i in 0..t.grad.len() {
                            let y = current.data[i];
                            t.grad[i] += grad[i] * (1.0 - y * y);
                        }
                    }
                }
            }
            TensorOp::Exp(a) => {
                let mut store = tensors().lock().map_err(|_| "tensor registry poisoned")?;
                if let Some(t) = store.get_mut(&a) {
                    if t.requires_grad {
                        for i in 0..t.grad.len() {
                            t.grad[i] += grad[i] * current.data[i];
                        }
                    }
                }
            }
            TensorOp::Log(a) => {
                let parent = tensor_snapshot(a)?;
                let mut store = tensors().lock().map_err(|_| "tensor registry poisoned")?;
                if let Some(t) = store.get_mut(&a) {
                    if t.requires_grad {
                        for i in 0..t.grad.len() {
                            t.grad[i] += grad[i] / parent.data[i];
                        }
                    }
                }
            }
            TensorOp::Reshape(a) => {
                let mut store = tensors().lock().map_err(|_| "tensor registry poisoned")?;
                if let Some(t) = store.get_mut(&a) {
                    if t.requires_grad {
                        for (dst, src) in t.grad.iter_mut().zip(grad.iter()) {
                            *dst += *src;
                        }
                    }
                }
            }
            TensorOp::Transpose(a, rows, cols) => {
                let mut incoming = vec![0.0; rows * cols];
                for r in 0..rows {
                    for c in 0..cols {
                        incoming[r * cols + c] = grad[c * rows + r];
                    }
                }
                let mut store = tensors().lock().map_err(|_| "tensor registry poisoned")?;
                if let Some(t) = store.get_mut(&a) {
                    if t.requires_grad {
                        for (dst, src) in t.grad.iter_mut().zip(incoming) {
                            *dst += src;
                        }
                    }
                }
            }
            TensorOp::Sum(a) => {
                let mut store = tensors().lock().map_err(|_| "tensor registry poisoned")?;
                if let Some(t) = store.get_mut(&a) {
                    if t.requires_grad {
                        for dst in &mut t.grad {
                            *dst += grad[0];
                        }
                    }
                }
            }
            TensorOp::Mean(a) => {
                let mut store = tensors().lock().map_err(|_| "tensor registry poisoned")?;
                if let Some(t) = store.get_mut(&a) {
                    if t.requires_grad && !t.grad.is_empty() {
                        let scale = grad[0] / t.grad.len() as f64;
                        for dst in &mut t.grad {
                            *dst += scale;
                        }
                    }
                }
            }
            TensorOp::MatMul(a, b, m, k, n) => {
                let left = tensor_snapshot(a)?;
                let right = tensor_snapshot(b)?;
                let mut grad_a = vec![0.0; m * k];
                let mut grad_b = vec![0.0; k * n];
                for i in 0..m {
                    let g_row = &grad[i * n..(i + 1) * n];
                    for p in 0..k {
                        let l_ip = left.data[i * k + p];
                        let r_row = &right.data[p * n..(p + 1) * n];
                        let gb_row = &mut grad_b[p * n..(p + 1) * n];
                        let mut sum_a = 0.0;
                        for j in 0..n {
                            let g = g_row[j];
                            sum_a += g * r_row[j];
                            gb_row[j] += g * l_ip;
                        }
                        grad_a[i * k + p] += sum_a;
                    }
                }
                let mut store = tensors().lock().map_err(|_| "tensor registry poisoned")?;
                if let Some(t) = store.get_mut(&a) {
                    if t.requires_grad {
                        for (dst, src) in t.grad.iter_mut().zip(grad_a) {
                            *dst += src;
                        }
                    }
                }
                if let Some(t) = store.get_mut(&b) {
                    if t.requires_grad {
                        for (dst, src) in t.grad.iter_mut().zip(grad_b) {
                            *dst += src;
                        }
                    }
                }
            }
            TensorOp::Scale(a, k) => {
                let mut store = tensors().lock().map_err(|_| "tensor registry poisoned")?;
                if let Some(t) = store.get_mut(&a) {
                    if t.requires_grad {
                        for (dst, src) in t.grad.iter_mut().zip(grad.iter()) {
                            *dst += *src * k;
                        }
                    }
                }
            }
            TensorOp::CrossEntropy(a, targets, rows, cols) => {
                let parent = tensor_snapshot(a)?;
                let probs = softmax_rows(&parent.data, rows, cols);
                let scale = grad[0] / rows as f64;
                let mut store = tensors().lock().map_err(|_| "tensor registry poisoned")?;
                if let Some(t) = store.get_mut(&a) {
                    if t.requires_grad {
                        for r in 0..rows {
                            for c in 0..cols {
                                let onehot = if targets[r] == c { 1.0 } else { 0.0 };
                                t.grad[r * cols + c] += scale * (probs[r * cols + c] - onehot);
                            }
                        }
                    }
                }
            }
            TensorOp::LayerNorm(a, rows, cols, eps) => {
                let parent = tensor_snapshot(a)?;
                let stats = layer_norm_stats(&parent.data, rows, cols, eps);
                let mut incoming = vec![0.0; rows * cols];
                for r in 0..rows {
                    let (mean, inv_std) = stats[r];
                    let base = r * cols;
                    let mut g_mean = 0.0;
                    let mut gx_mean = 0.0;
                    for c in 0..cols {
                        let xhat = (parent.data[base + c] - mean) * inv_std;
                        g_mean += grad[base + c];
                        gx_mean += grad[base + c] * xhat;
                    }
                    g_mean /= cols as f64;
                    gx_mean /= cols as f64;
                    for c in 0..cols {
                        let xhat = (parent.data[base + c] - mean) * inv_std;
                        incoming[base + c] = inv_std * (grad[base + c] - g_mean - xhat * gx_mean);
                    }
                }
                let mut store = tensors().lock().map_err(|_| "tensor registry poisoned")?;
                if let Some(t) = store.get_mut(&a) {
                    if t.requires_grad {
                        for (dst, src) in t.grad.iter_mut().zip(incoming) {
                            *dst += src;
                        }
                    }
                }
            }
            TensorOp::Conv2d(a, b, d) => {
                let input = tensor_snapshot(a)?;
                let weight = tensor_snapshot(b)?;
                let mut grad_in = vec![0.0; input.data.len()];
                let mut grad_w = vec![0.0; weight.data.len()];
                for n in 0..d.n {
                    for o in 0..d.o {
                        for y in 0..d.oh {
                            for x in 0..d.ow {
                                let g = grad[d.out_idx(n, o, y, x)];
                                if g == 0.0 {
                                    continue;
                                }
                                for c in 0..d.c {
                                    for i in 0..d.kh {
                                        for j in 0..d.kw {
                                            let ii =
                                                d.in_idx(n, c, y * d.stride + i, x * d.stride + j);
                                            let wi = d.w_idx(o, c, i, j);
                                            grad_in[ii] += g * weight.data[wi];
                                            grad_w[wi] += g * input.data[ii];
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                let mut store = tensors().lock().map_err(|_| "tensor registry poisoned")?;
                if let Some(t) = store.get_mut(&a) {
                    if t.requires_grad {
                        for (dst, src) in t.grad.iter_mut().zip(grad_in) {
                            *dst += src;
                        }
                    }
                }
                if let Some(t) = store.get_mut(&b) {
                    if t.requires_grad {
                        for (dst, src) in t.grad.iter_mut().zip(grad_w) {
                            *dst += src;
                        }
                    }
                }
            }
            TensorOp::SliceCols(a, rows, total_cols, col_start, col_len) => {
                let mut store = tensors().lock().map_err(|_| "tensor registry poisoned")?;
                if let Some(t) = store.get_mut(&a) {
                    if t.requires_grad {
                        for r in 0..rows {
                            for c in 0..col_len {
                                t.grad[r * total_cols + col_start + c] += grad[r * col_len + c];
                            }
                        }
                    }
                }
            }
            TensorOp::ConcatCols(parents, rows, col_lens) => {
                let total_cols: usize = col_lens.iter().sum();
                let mut store = tensors().lock().map_err(|_| "tensor registry poisoned")?;
                let mut col_offset = 0;
                for (pid, parent_cols) in parents.into_iter().zip(col_lens.into_iter()) {
                    if let Some(t) = store.get_mut(&pid) {
                        if t.requires_grad {
                            for r in 0..rows {
                                for c in 0..parent_cols {
                                    t.grad[r * parent_cols + c] +=
                                        grad[r * total_cols + col_offset + c];
                                }
                            }
                        }
                    }
                    col_offset += parent_cols;
                }
            }
        }
    }
    PENDING_TAPE_IDS.with(|cell| {
        let mut pending = cell.borrow_mut();
        for id in order {
            if let Ok(t) = tensor_snapshot(id) {
                if !matches!(t.op, TensorOp::Leaf) {
                    pending.insert(id);
                }
            }
        }
    });
    Ok(())
}

pub(crate) fn collect_val_tensor_ids(val: &Val, out: &mut HashSet<u64>) {
    match val {
        Val::Map(_) => {
            if let Ok(id) = tensor_id(val) {
                out.insert(id);
            } else if let Val::Map(pairs) = val {
                for (k, v) in pairs {
                    collect_val_tensor_ids(k, out);
                    collect_val_tensor_ids(v, out);
                }
            }
        }
        Val::List(items) => {
            for item in items {
                collect_val_tensor_ids(item, out);
            }
        }
        _ => {}
    }
}

pub(crate) fn tensor_gc_unreachable(live_roots: &HashSet<u64>) -> Result<usize, String> {
    let mut reachable = HashSet::new();
    for &root in live_roots {
        let mut order = vec![];
        let _ = tensor_topology(root, &mut reachable, &mut order);
    }
    let mut store = tensors().lock().map_err(|_| "tensor registry poisoned")?;
    let mut removed = 0usize;
    PENDING_TAPE_IDS.with(|cell| {
        let mut pending = cell.borrow_mut();
        pending.retain(|id| {
            if reachable.contains(id) {
                true
            } else {
                if store.remove(id).is_some() {
                    removed += 1;
                }
                false
            }
        });
    });
    Ok(removed)
}
