// Bytecode IR optimization pipeline (Veyra 12A):
// - Constant folding for arithmetic, power, string concatenation, equality, relational, and unary ops
// - Redundant double-negation elimination on boolean expressions
// - Dead constant-pop and constant conditional-branch elimination
// - Jump-target remapping and jump-to-jump threading

use std::collections::HashSet;

use crate::bytecode::Op;
use crate::value::Val;

pub(crate) fn optimize_bytecode(code: Vec<Op>) -> Vec<Op> {
    let mut current: Vec<Op> = code
        .into_iter()
        .map(|op| match op {
            Op::Def(name, params, ret, is_async, body) => {
                Op::Def(name, params, ret, is_async, optimize_bytecode(body))
            }
            Op::DefClass(name, parent, methods) => {
                let opt_methods = methods
                    .into_iter()
                    .map(|(m, p, r, a, body)| (m, p, r, a, optimize_bytecode(body)))
                    .collect();
                Op::DefClass(name, parent, opt_methods)
            }
            other => other,
        })
        .collect();

    for _ in 0..8 {
        let next = optimize_pass(&current);
        if next.len() == current.len() && ops_equivalent(&next, &current) {
            break;
        }
        current = next;
    }
    thread_jumps(&mut current);
    current
}

fn ops_equivalent(a: &[Op], b: &[Op]) -> bool {
    a.len() == b.len() && format!("{a:?}") == format!("{b:?}")
}

fn collect_jump_targets(code: &[Op]) -> HashSet<usize> {
    let mut targets = HashSet::new();
    for op in code {
        match op {
            Op::Jump(t) | Op::JumpIfFalse(t) | Op::PushHandler(t, _) => {
                targets.insert(*t);
            }
            _ => {}
        }
    }
    targets
}

fn is_bool_producing(op: &Op) -> bool {
    matches!(
        op,
        Op::Eq | Op::Ne | Op::Lt | Op::Le | Op::Gt | Op::Ge | Op::Not | Op::Const(Val::Bool(_))
    )
}

fn fold_binary(a: &Val, b: &Val, op: &Op) -> Option<Val> {
    match (a, b, op) {
        (Val::Num(x), Val::Num(y), Op::Add) => Some(Val::Num(x + y)),
        (Val::Str(x), Val::Str(y), Op::Add) => Some(Val::Str(format!("{x}{y}"))),
        (Val::Num(x), Val::Num(y), Op::Sub) => Some(Val::Num(x - y)),
        (Val::Num(x), Val::Num(y), Op::Mul) => Some(Val::Num(x * y)),
        (Val::Num(x), Val::Num(y), Op::Div) if *y != 0.0 => Some(Val::Num(x / y)),
        (Val::Num(x), Val::Num(y), Op::Mod) if *y != 0.0 => Some(Val::Num(x % y)),
        (Val::Num(x), Val::Num(y), Op::Pow) => Some(Val::Num(x.powf(*y))),
        (x, y, Op::Eq) => Some(Val::Bool(x == y)),
        (x, y, Op::Ne) => Some(Val::Bool(x != y)),
        (Val::Num(x), Val::Num(y), Op::Lt) => Some(Val::Bool(x < y)),
        (Val::Num(x), Val::Num(y), Op::Le) => Some(Val::Bool(x <= y)),
        (Val::Num(x), Val::Num(y), Op::Gt) => Some(Val::Bool(x > y)),
        (Val::Num(x), Val::Num(y), Op::Ge) => Some(Val::Bool(x >= y)),
        _ => None,
    }
}

fn optimize_pass(code: &[Op]) -> Vec<Op> {
    let targets = collect_jump_targets(code);
    let mut out = Vec::with_capacity(code.len());
    let mut old_to_new = vec![0usize; code.len() + 1];
    let mut i = 0;

    while i < code.len() {
        let new_idx = out.len();
        old_to_new[i] = new_idx;

        if i + 2 < code.len() && !targets.contains(&(i + 1)) && !targets.contains(&(i + 2)) {
            if let (Op::Const(a), Op::Const(b), op) = (&code[i], &code[i + 1], &code[i + 2]) {
                if let Some(folded) = fold_binary(a, b, op) {
                    old_to_new[i + 1] = new_idx;
                    old_to_new[i + 2] = new_idx;
                    out.push(Op::Const(folded));
                    i += 3;
                    continue;
                }
            }
            if is_bool_producing(&code[i])
                && matches!(code[i + 1], Op::Not)
                && matches!(code[i + 2], Op::Not)
            {
                old_to_new[i + 1] = new_idx;
                old_to_new[i + 2] = new_idx;
                out.push(code[i].clone());
                i += 3;
                continue;
            }
        }

        if i + 1 < code.len() && !targets.contains(&(i + 1)) {
            match (&code[i], &code[i + 1]) {
                (Op::Const(Val::Num(x)), Op::Neg) => {
                    old_to_new[i + 1] = new_idx;
                    out.push(Op::Const(Val::Num(-*x)));
                    i += 2;
                    continue;
                }
                (Op::Const(v), Op::Not) => {
                    old_to_new[i + 1] = new_idx;
                    out.push(Op::Const(Val::Bool(!v.truthy())));
                    i += 2;
                    continue;
                }
                (Op::Const(_), Op::Pop) => {
                    old_to_new[i + 1] = new_idx;
                    i += 2;
                    continue;
                }
                (Op::Const(v), Op::JumpIfFalse(target)) => {
                    old_to_new[i + 1] = new_idx;
                    if !v.truthy() {
                        out.push(Op::Jump(*target));
                    }
                    i += 2;
                    continue;
                }
                _ => {}
            }
        }

        out.push(code[i].clone());
        i += 1;
    }
    old_to_new[code.len()] = out.len();

    for op in &mut out {
        match op {
            Op::Jump(t) | Op::JumpIfFalse(t) | Op::PushHandler(t, _) => {
                if *t < old_to_new.len() {
                    *t = old_to_new[*t];
                }
            }
            _ => {}
        }
    }
    out
}

fn thread_jumps(code: &mut [Op]) {
    let len = code.len();
    for i in 0..len {
        let mut hops = 0;
        while hops < 8 {
            let target = match code[i] {
                Op::Jump(t) | Op::JumpIfFalse(t) => Some(t),
                _ => None,
            };
            let Some(t) = target else { break };
            if t >= len {
                break;
            }
            if let Op::Jump(next_t) = code[t] {
                if next_t != t {
                    match &mut code[i] {
                        Op::Jump(dst) | Op::JumpIfFalse(dst) => *dst = next_t,
                        _ => {}
                    }
                    hops += 1;
                    continue;
                }
            }
            break;
        }
    }
}
