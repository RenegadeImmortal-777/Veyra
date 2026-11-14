// Multi-pass bytecode IR optimizer:
// - Constant folding for arithmetic, power, string concatenation, equality, relational, and unary ops
// - Intra-block scalar local constant propagation
// - Algebraic identity & strength reduction on pure locals/constants
// - Redundant double-negation elimination, fallthrough jump elision & jump-to-jump threading
// - Reachable basic-block dead-code elimination (DCE)

use std::collections::{HashMap, HashSet, VecDeque};

use crate::bytecode::Op;
use crate::value::Val;

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct IrStats {
    pub(crate) ops_before: usize,
    pub(crate) ops_after: usize,
    pub(crate) constants_folded: usize,
    pub(crate) algebraic_simplified: usize,
    pub(crate) dead_removed: usize,
}

fn count_ops_recursive(code: &[Op]) -> usize {
    let mut n = code.len();
    for op in code {
        match op {
            Op::Def(_, _, _, _, body) | Op::BuildClass(_, _, body) => {
                n += count_ops_recursive(body);
            }
            Op::DefClass(_, _, methods) => {
                for (_, _, _, _, body) in methods {
                    n += count_ops_recursive(body);
                }
            }
            _ => {}
        }
    }
    n
}

pub(crate) fn optimize_bytecode(code: Vec<Op>) -> Vec<Op> {
    optimize_bytecode_with_stats(code).0
}

pub(crate) fn optimize_bytecode_with_stats(code: Vec<Op>) -> (Vec<Op>, IrStats) {
    optimize_inner(code, &HashSet::new())
}

fn optimize_inner(code: Vec<Op>, seed_num_slots: &HashSet<usize>) -> (Vec<Op>, IrStats) {
    let ops_before = count_ops_recursive(&code);
    let mut stats = IrStats {
        ops_before,
        ..Default::default()
    };

    let mut current: Vec<Op> = code
        .into_iter()
        .map(|op| match op {
            Op::Def(name, params, ret, is_async, body) => {
                let mut param_nums = HashSet::new();
                for (idx, (_, ty)) in params.iter().enumerate() {
                    if matches!(ty.as_deref(), Some("Int") | Some("Float") | Some("Number")) {
                        param_nums.insert(idx);
                    }
                }
                let (opt_body, sub) = optimize_inner(body, &param_nums);
                stats.constants_folded += sub.constants_folded;
                stats.algebraic_simplified += sub.algebraic_simplified;
                stats.dead_removed += sub.dead_removed;
                Op::Def(name, params, ret, is_async, opt_body)
            }
            Op::DefClass(name, parent, methods) => {
                let opt_methods = methods
                    .into_iter()
                    .map(|(m, p, r, a, body)| {
                        let mut param_nums = HashSet::new();
                        for (idx, (_, ty)) in p.iter().enumerate() {
                            if matches!(ty.as_deref(), Some("Int") | Some("Float") | Some("Number"))
                            {
                                param_nums.insert(idx);
                            }
                        }
                        let (opt_body, sub) = optimize_inner(body, &param_nums);
                        stats.constants_folded += sub.constants_folded;
                        stats.algebraic_simplified += sub.algebraic_simplified;
                        stats.dead_removed += sub.dead_removed;
                        (m, p, r, a, opt_body)
                    })
                    .collect();
                Op::DefClass(name, parent, opt_methods)
            }
            Op::BuildClass(name, base_count, body) => {
                let (opt_body, sub) = optimize_inner(body, &HashSet::new());
                stats.constants_folded += sub.constants_folded;
                stats.algebraic_simplified += sub.algebraic_simplified;
                stats.dead_removed += sub.dead_removed;
                Op::BuildClass(name, base_count, opt_body)
            }
            other => other,
        })
        .collect();

    for _ in 0..10 {
        let (propagated, prop_n) = propagate_local_constants(&current);
        let num_slots = infer_pure_num_slots(&propagated, seed_num_slots);
        let (next, pass_stats) = optimize_pass(&propagated, &num_slots);
        let (pruned, dce_n) = eliminate_unreachable(next);
        stats.constants_folded += prop_n + pass_stats.constants_folded;
        stats.algebraic_simplified += pass_stats.algebraic_simplified;
        stats.dead_removed += pass_stats.dead_removed + dce_n;
        if pruned.len() == current.len() && ops_equivalent(&pruned, &current) {
            current = pruned;
            break;
        }
        current = pruned;
    }
    thread_jumps(&mut current);
    let (final_code, final_dce) = eliminate_unreachable(current);
    stats.dead_removed += final_dce;
    stats.ops_after = count_ops_recursive(&final_code);
    (final_code, stats)
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
        (Val::List(x), Val::List(y), Op::Add) if x.len() + y.len() <= 256 => {
            let mut out = x.clone();
            out.extend(y.iter().cloned());
            Some(Val::List(out))
        }
        (Val::Num(x), Val::Num(y), Op::Sub) => Some(Val::Num(x - y)),
        (Val::Num(x), Val::Num(y), Op::Mul) => Some(Val::Num(x * y)),
        (Val::Str(s), Val::Num(n), Op::Mul) | (Val::Num(n), Val::Str(s), Op::Mul)
            if *n >= 0.0 && (*n as usize).saturating_mul(s.len()) <= 1024 =>
        {
            Some(Val::Str(s.repeat(*n as usize)))
        }
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

fn is_scalar_const(v: &Val) -> bool {
    matches!(v, Val::Num(_) | Val::Bool(_) | Val::Null | Val::Str(_))
}

fn propagate_local_constants(code: &[Op]) -> (Vec<Op>, usize) {
    let targets = collect_jump_targets(code);
    let mut out = code.to_vec();
    let mut env: HashMap<usize, Val> = HashMap::new();
    let mut last_const: Option<Val> = None;
    let mut replaced = 0usize;

    for (i, op) in code.iter().enumerate() {
        if targets.contains(&i) {
            env.clear();
            last_const = None;
        }
        match op {
            Op::Const(v) if is_scalar_const(v) => {
                last_const = Some(v.clone());
                continue;
            }
            Op::DefineLocal(s, _, _) | Op::StoreLocal(s) => {
                if let Some(c) = last_const.take() {
                    env.insert(*s, c);
                } else {
                    env.remove(s);
                }
                continue;
            }
            Op::LoadLocal(s) => {
                if let Some(c) = env.get(s) {
                    out[i] = Op::Const(c.clone());
                    last_const = Some(c.clone());
                    replaced += 1;
                    continue;
                }
            }
            Op::Jump(_)
            | Op::JumpIfFalse(_)
            | Op::PushHandler(_, _)
            | Op::PopHandler
            | Op::Return
            | Op::Throw
            | Op::Halt
            | Op::Call(..)
            | Op::CallExpr(_)
            | Op::CallMethod(..)
            | Op::CallExt(..)
            | Op::Yield => {
                env.clear();
            }
            _ => {}
        }
        last_const = None;
    }
    (out, replaced)
}

fn infer_pure_num_slots(code: &[Op], seed: &HashSet<usize>) -> HashSet<usize> {
    let mut num_slots: HashSet<usize> = seed.clone();
    let mut non_num_slots: HashSet<usize> = HashSet::new();
    let targets = collect_jump_targets(code);
    for op in code {
        if let Op::DefineLocal(s, _, Some(ty)) = op
            && matches!(ty.as_str(), "Int" | "Float" | "Number")
        {
            num_slots.insert(*s);
        }
    }
    // Monotonic worklist over local slots: once a slot observes any potentially non-numeric store
    // (unless explicitly seeded/typed numeric), it stays in `non_num_slots` so fixed-point converges.
    let mut changed = true;
    while changed {
        changed = false;
        let mut stack: Vec<bool> = Vec::new();
        for (i, op) in code.iter().enumerate() {
            if targets.contains(&i) {
                stack.clear();
            }
            match op {
                Op::Const(Val::Num(_)) => stack.push(true),
                Op::Const(_) | Op::Load(_) | Op::FunctionRef(_) => stack.push(false),
                Op::LoadLocal(s) => stack.push(num_slots.contains(s) && !non_num_slots.contains(s)),
                Op::DefineLocal(s, _, ty) => {
                    let top_num = stack.pop().unwrap_or(false);
                    let typed_num =
                        matches!(ty.as_deref(), Some("Int") | Some("Float") | Some("Number"))
                            || seed.contains(s);
                    if !top_num && !typed_num {
                        if num_slots.remove(s) {
                            changed = true;
                        }
                        if non_num_slots.insert(*s) {
                            changed = true;
                        }
                    } else if (top_num || typed_num)
                        && !non_num_slots.contains(s)
                        && num_slots.insert(*s)
                    {
                        changed = true;
                    }
                }
                Op::StoreLocal(s) => {
                    let top_num = stack.pop().unwrap_or(false);
                    if !top_num && !seed.contains(s) {
                        if num_slots.remove(s) {
                            changed = true;
                        }
                        if non_num_slots.insert(*s) {
                            changed = true;
                        }
                    }
                }
                Op::Neg => {
                    let a = stack.pop().unwrap_or(false);
                    stack.push(a);
                }
                Op::Sub | Op::Div | Op::Mod | Op::Pow | Op::Add | Op::Mul => {
                    let b = stack.pop().unwrap_or(false);
                    let a = stack.pop().unwrap_or(false);
                    stack.push(a && b);
                }
                Op::Eq | Op::Ne | Op::Lt | Op::Le | Op::Gt | Op::Ge | Op::Not => {
                    match op {
                        Op::Not => {
                            let _ = stack.pop();
                        }
                        _ => {
                            let _ = stack.pop();
                            let _ = stack.pop();
                        }
                    }
                    stack.push(false);
                }
                Op::Pop | Op::JumpIfFalse(_) | Op::Return | Op::Yield | Op::Throw => {
                    let _ = stack.pop();
                }
                Op::Call(_, argc) | Op::CallExpr(argc) | Op::CallMethod(_, argc) => {
                    for _ in 0..*argc {
                        let _ = stack.pop();
                    }
                    if matches!(op, Op::CallMethod(..)) {
                        let _ = stack.pop();
                    }
                    if matches!(op, Op::CallExpr(..)) {
                        let _ = stack.pop();
                    }
                    stack.push(false);
                }
                Op::CallExt(_, _, specs) => {
                    for _ in specs {
                        let _ = stack.pop();
                    }
                    stack.push(false);
                }
                Op::Index | Op::GetMember(_) => {
                    if matches!(op, Op::Index) {
                        let _ = stack.pop();
                    }
                    let _ = stack.pop();
                    stack.push(false);
                }
                Op::SetMember(_) => {
                    let _ = stack.pop();
                    let _ = stack.pop();
                    stack.push(false);
                }
                Op::SetIndex => {
                    let _ = stack.pop();
                    let _ = stack.pop();
                    let _ = stack.pop();
                    stack.push(false);
                }
                Op::Jump(_) | Op::Halt | Op::PushHandler(_, _) | Op::PopHandler => {}
                _ => stack.clear(),
            }
        }
    }
    num_slots
}

fn try_simplify_algebraic(
    a: &Op,
    b: &Op,
    op: &Op,
    num_slots: &HashSet<usize>,
) -> Option<[Option<Op>; 3]> {
    match (a, b, op) {
        (Op::LoadLocal(s), Op::Const(Val::Num(n)), Op::Add | Op::Sub)
            if *n == 0.0 && num_slots.contains(s) =>
        {
            Some([Some(Op::LoadLocal(*s)), None, None])
        }
        (Op::Const(Val::Num(n)), Op::LoadLocal(s), Op::Add)
            if *n == 0.0 && num_slots.contains(s) =>
        {
            Some([Some(Op::LoadLocal(*s)), None, None])
        }
        (Op::LoadLocal(s), Op::Const(Val::Num(n)), Op::Mul | Op::Div | Op::Pow)
            if *n == 1.0 && num_slots.contains(s) =>
        {
            Some([Some(Op::LoadLocal(*s)), None, None])
        }
        (Op::Const(Val::Num(n)), Op::LoadLocal(s), Op::Mul)
            if *n == 1.0 && num_slots.contains(s) =>
        {
            Some([Some(Op::LoadLocal(*s)), None, None])
        }
        (Op::LoadLocal(s), Op::Const(Val::Num(n)), Op::Mul)
        | (Op::Const(Val::Num(n)), Op::LoadLocal(s), Op::Mul)
            if *n == 0.0 && num_slots.contains(s) =>
        {
            Some([Some(Op::Const(Val::Num(0.0))), None, None])
        }
        (Op::LoadLocal(s), Op::Const(Val::Num(n)), Op::Pow)
            if *n == 0.0 && num_slots.contains(s) =>
        {
            Some([Some(Op::Const(Val::Num(1.0))), None, None])
        }
        (Op::LoadLocal(s), Op::Const(Val::Num(n)), Op::Pow)
            if *n == 2.0 && num_slots.contains(s) =>
        {
            Some([
                Some(Op::LoadLocal(*s)),
                Some(Op::LoadLocal(*s)),
                Some(Op::Mul),
            ])
        }
        (Op::LoadLocal(x), Op::LoadLocal(y), Op::Sub) if x == y && num_slots.contains(x) => {
            Some([Some(Op::Const(Val::Num(0.0))), None, None])
        }
        (Op::LoadLocal(x), Op::LoadLocal(y), Op::Eq | Op::Le | Op::Ge)
            if x == y && num_slots.contains(x) =>
        {
            Some([Some(Op::Const(Val::Bool(true))), None, None])
        }
        (Op::LoadLocal(x), Op::LoadLocal(y), Op::Ne | Op::Lt | Op::Gt)
            if x == y && num_slots.contains(x) =>
        {
            Some([Some(Op::Const(Val::Bool(false))), None, None])
        }
        _ => None,
    }
}

fn remap_jumps(code: &mut [Op], old_to_new: &[usize]) {
    for op in code {
        match op {
            Op::Jump(t) | Op::JumpIfFalse(t) | Op::PushHandler(t, _) if *t < old_to_new.len() => {
                *t = old_to_new[*t];
            }
            _ => {}
        }
    }
}

fn optimize_pass(code: &[Op], num_slots: &HashSet<usize>) -> (Vec<Op>, IrStats) {
    let targets = collect_jump_targets(code);
    let mut out = Vec::with_capacity(code.len());
    let mut old_to_new = vec![0usize; code.len() + 1];
    let mut stats = IrStats::default();
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
                    stats.constants_folded += 1;
                    i += 3;
                    continue;
                }
            }
            if let Some(rep) =
                try_simplify_algebraic(&code[i], &code[i + 1], &code[i + 2], num_slots)
            {
                if rep[1].is_none() {
                    old_to_new[i + 1] = new_idx;
                    old_to_new[i + 2] = new_idx;
                    out.push(rep[0].clone().unwrap());
                } else {
                    old_to_new[i + 1] = new_idx + 1;
                    old_to_new[i + 2] = new_idx + 2;
                    for slot in rep.into_iter().flatten() {
                        out.push(slot);
                    }
                }
                stats.algebraic_simplified += 1;
                i += 3;
                continue;
            }
            if is_bool_producing(&code[i])
                && matches!(code[i + 1], Op::Not)
                && matches!(code[i + 2], Op::Not)
            {
                old_to_new[i + 1] = new_idx;
                old_to_new[i + 2] = new_idx;
                out.push(code[i].clone());
                stats.algebraic_simplified += 1;
                i += 3;
                continue;
            }
        }

        if i + 1 < code.len() && !targets.contains(&(i + 1)) {
            match (&code[i], &code[i + 1]) {
                (Op::Const(Val::Num(x)), Op::Neg) => {
                    old_to_new[i + 1] = new_idx;
                    out.push(Op::Const(Val::Num(-*x)));
                    stats.constants_folded += 1;
                    i += 2;
                    continue;
                }
                (Op::Const(v), Op::Not) => {
                    old_to_new[i + 1] = new_idx;
                    out.push(Op::Const(Val::Bool(!v.truthy())));
                    stats.constants_folded += 1;
                    i += 2;
                    continue;
                }
                (Op::Const(_), Op::Pop) => {
                    old_to_new[i + 1] = new_idx;
                    stats.dead_removed += 1;
                    i += 2;
                    continue;
                }
                (Op::Const(v), Op::JumpIfFalse(target)) => {
                    old_to_new[i + 1] = new_idx;
                    if !v.truthy() {
                        out.push(Op::Jump(*target));
                    }
                    stats.dead_removed += 1;
                    i += 2;
                    continue;
                }
                _ => {}
            }
        }

        if let Op::Jump(t) = code[i]
            && t == i + 1
        {
            stats.dead_removed += 1;
            i += 1;
            continue;
        }

        out.push(code[i].clone());
        i += 1;
    }
    old_to_new[code.len()] = out.len();
    remap_jumps(&mut out, &old_to_new);
    (out, stats)
}

fn eliminate_unreachable(code: Vec<Op>) -> (Vec<Op>, usize) {
    if code.is_empty() {
        return (code, 0);
    }
    let n = code.len();
    let mut reachable = vec![false; n];
    let mut queue = VecDeque::new();
    reachable[0] = true;
    queue.push_back(0usize);

    while let Some(ip) = queue.pop_front() {
        let mut mark = |dst: usize| {
            if dst < n && !reachable[dst] {
                reachable[dst] = true;
                queue.push_back(dst);
            }
        };
        match &code[ip] {
            Op::Jump(t) => mark(*t),
            Op::JumpIfFalse(t) | Op::PushHandler(t, _) => {
                mark(*t);
                mark(ip + 1);
            }
            Op::Return | Op::Throw | Op::Halt => {}
            _ => mark(ip + 1),
        }
    }

    let removed = reachable.iter().filter(|&&r| !r).count();
    if removed == 0 {
        return (code, 0);
    }
    let mut old_to_new = vec![0usize; n + 1];
    let mut out = Vec::with_capacity(n - removed);
    for (i, op) in code.into_iter().enumerate() {
        old_to_new[i] = out.len();
        if reachable[i] {
            out.push(op);
        }
    }
    old_to_new[n] = out.len();
    remap_jumps(&mut out, &old_to_new);
    (out, removed)
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

/// Compile-time escape analysis over compiled bytecode: determines whether a function
/// may allocate objects/lists without escaping through global mutation, generators,
/// closures, or concurrent actors, making it safe for automatic VM arena framing.
pub(crate) fn code_eligible_for_auto_arena(code: &[Op]) -> bool {
    let mut may_alloc = false;
    for op in code {
        match op {
            Op::Yield
            | Op::Store(_)
            | Op::Def(_, _, _, _, _)
            | Op::DefClass(_, _, _)
            | Op::BuildClass(_, _, _)
            | Op::FunctionRef(_) => return false,
            Op::Call(name, _) => {
                if matches!(
                    name.as_str(),
                    "actor_spawn"
                        | "actor_send"
                        | "actor_ask"
                        | "chan_send"
                        | "channel_send"
                        | "sim_actor_spawn"
                        | "sim_actor_send"
                        | "supervisor_spawn"
                        | "arena_scope"
                        | "spawn"
                        | "global_set"
                ) {
                    return false;
                }
                if !matches!(
                    name.as_str(),
                    "abs"
                        | "min"
                        | "max"
                        | "sqrt"
                        | "sin"
                        | "cos"
                        | "tan"
                        | "exp"
                        | "log"
                        | "log2"
                        | "pow"
                        | "floor"
                        | "ceil"
                        | "round"
                        | "len"
                        | "int"
                        | "float"
                        | "bool"
                        | "assert"
                        | "requires"
                        | "ensures"
                ) {
                    may_alloc = true;
                }
            }
            Op::CallExpr(_) | Op::CallMethod(_, _) | Op::CallExt(_, _, _) => {
                may_alloc = true;
            }
            _ => {}
        }
    }
    may_alloc
}
