// Tensor runtime: handles, shape/autograd topology and the reverse-mode
// backward pass behind the `tensor_*` builtins.
//
// Extracted from `main.rs` without behavior change; see docs/ENGINE-SPLIT.md.

use std::collections::{HashMap, HashSet};
use std::sync::{
    Mutex, OnceLock,
    atomic::{AtomicU64, Ordering},
};

use crate::value::{Val, as_matrix, as_num_list};

pub(crate) static TENSOR_IDS: AtomicU64 = AtomicU64::new(1);
pub(crate) static TENSORS: OnceLock<Mutex<HashMap<u64, TensorData>>> = OnceLock::new();
pub(crate) static ADAM_STATES: OnceLock<Mutex<HashMap<u64, AdamState>>> = OnceLock::new();

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

pub(crate) fn tensor_from_value(value: &Val, requires_grad: bool) -> Result<Val, String> {
    let (data, shape) = match value {
        Val::List(rows) if rows.iter().all(|x| matches!(x, Val::List(_))) => {
            let matrix = as_matrix(value)?;
            if matrix.is_empty() {
                (vec![], vec![0, 0])
            } else {
                let cols = matrix[0].len();
                if matrix.iter().any(|r| r.len() != cols) {
                    return Err("tensor matrix rows must have equal length".into());
                }
                (
                    matrix.into_iter().flatten().collect(),
                    vec![rows.len(), cols],
                )
            }
        }
        Val::List(_) => {
            let xs = as_num_list(value)?;
            let n = xs.len();
            (xs, vec![n])
        }
        Val::Num(n) => (vec![*n], vec![]),
        _ => return Err("tensor expects Number or numeric List".into()),
    };
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
        | TensorOp::Mean(a) => vec![a],
        TensorOp::MatMul(a, b, _, _, _) => vec![a, b],
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
                for g in &mut tensor.grad {
                    *g = 0.0;
                }
            }
        }
        let t = store.get_mut(&root).ok_or("unknown tensor")?;
        t.grad = vec![1.0];
    }
    for id in order.into_iter().rev() {
        let current = tensor_snapshot(id)?;
        let grad = current.grad.clone();
        match current.op {
            TensorOp::Leaf => {}
            TensorOp::Add(a, b) => {
                let mut store = tensors().lock().map_err(|_| "tensor registry poisoned")?;
                for parent in [a, b] {
                    if let Some(t) = store.get_mut(&parent) {
                        if t.requires_grad {
                            for (dst, src) in t.grad.iter_mut().zip(grad.iter()) {
                                *dst += *src;
                            }
                        }
                    }
                }
            }
            TensorOp::Sub(a, b) => {
                let mut store = tensors().lock().map_err(|_| "tensor registry poisoned")?;
                if let Some(t) = store.get_mut(&a) {
                    if t.requires_grad {
                        for (dst, src) in t.grad.iter_mut().zip(grad.iter()) {
                            *dst += *src;
                        }
                    }
                }
                if let Some(t) = store.get_mut(&b) {
                    if t.requires_grad {
                        for (dst, src) in t.grad.iter_mut().zip(grad.iter()) {
                            *dst -= *src;
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
                        for i in 0..t.grad.len() {
                            t.grad[i] += grad[i] * right.data[i];
                        }
                    }
                }
                if let Some(t) = store.get_mut(&b) {
                    if t.requires_grad {
                        for i in 0..t.grad.len() {
                            t.grad[i] += grad[i] * left.data[i];
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
                        for i in 0..t.grad.len() {
                            t.grad[i] += grad[i] / right.data[i];
                        }
                    }
                }
                if let Some(t) = store.get_mut(&b) {
                    if t.requires_grad {
                        for i in 0..t.grad.len() {
                            let denom = right.data[i] * right.data[i];
                            t.grad[i] -= grad[i] * left.data[i] / denom;
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
                    for j in 0..n {
                        let g = grad[i * n + j];
                        for p in 0..k {
                            grad_a[i * k + p] += g * right.data[p * n + j];
                            grad_b[p * n + j] += g * left.data[i * k + p];
                        }
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
        }
    }
    Ok(())
}
