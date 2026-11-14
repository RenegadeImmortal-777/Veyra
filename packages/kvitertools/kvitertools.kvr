fn chain(lists: List) -> List {
    let out = [];
    for sub in lists {
        for item in sub {
            out = append(out, item);
        }
    }
    return out;
}

fn repeat(val: Any, count: Number) -> List {
    let out = [];
    let i = 0;
    while i < count {
        out = append(out, val);
        i = i + 1;
    }
    return out;
}

fn take(n: Number, xs: List) -> List {
    let out = [];
    let limit = n if n < len(xs) else len(xs);
    let i = 0;
    while i < limit {
        out = append(out, xs[i]);
        i = i + 1;
    }
    return out;
}

fn drop(n: Number, xs: List) -> List {
    let out = [];
    let i = n if n > 0 else 0;
    while i < len(xs) {
        out = append(out, xs[i]);
        i = i + 1;
    }
    return out;
}

fn islice(xs: List, start: Number, stop: Number, step: Number) -> List {
    if step <= 0 {
        throw "islice step must be > 0";
    }
    let out = [];
    let hi = stop if stop < len(xs) else len(xs);
    let i = start if start > 0 else 0;
    while i < hi {
        out = append(out, xs[i]);
        i = i + step;
    }
    return out;
}

fn accumulate(xs: List, fn_step: Function) -> List {
    if len(xs) == 0 {
        return [];
    }
    let acc = xs[0];
    let out = [acc];
    let i = 1;
    while i < len(xs) {
        acc = fn_step(acc, xs[i]);
        out = append(out, acc);
        i = i + 1;
    }
    return out;
}

fn batched(xs: List, size: Number) -> List {
    if size <= 0 {
        throw "batched size must be > 0";
    }
    let out = [];
    let i = 0;
    while i < len(xs) {
        let batch = [];
        let j = 0;
        while j < size and i + j < len(xs) {
            batch = append(batch, xs[i + j]);
            j = j + 1;
        }
        out = append(out, batch);
        i = i + size;
    }
    return out;
}

fn pairwise(xs: List) -> List {
    let out = [];
    let i = 0;
    while i + 1 < len(xs) {
        out = append(out, [xs[i], xs[i + 1]]);
        i = i + 1;
    }
    return out;
}

fn zip_longest(a: List, b: List, fillvalue: Any) -> List {
    let n = len(a) if len(a) > len(b) else len(b);
    let out = [];
    let i = 0;
    while i < n {
        let va = a[i] if i < len(a) else fillvalue;
        let vb = b[i] if i < len(b) else fillvalue;
        out = append(out, [va, vb]);
        i = i + 1;
    }
    return out;
}

fn compress(data: List, selectors: List) -> List {
    let n = len(data) if len(data) < len(selectors) else len(selectors);
    let out = [];
    let i = 0;
    while i < n {
        if selectors[i] {
            out = append(out, data[i]);
        }
        i = i + 1;
    }
    return out;
}

fn product2(a: List, b: List) -> List {
    let out = [];
    for x in a {
        for y in b {
            out = append(out, [x, y]);
        }
    }
    return out;
}

fn combinations(xs: List, r: Number) -> List {
    let k = int(r);
    if k == 0 {
        return [[]];
    }
    if k < 0 or k > len(xs) {
        return [];
    }
    let out = [];
    let i = 0;
    while i <= len(xs) - k {
        let head = xs[i];
        let tail = xs[(i + 1):];
        for sub in combinations(tail, k - 1) {
            out = append(out, [head] + sub);
        }
        i = i + 1;
    }
    return out;
}

fn permutations(xs: List, r: Number) -> List {
    let k = int(r);
    if k == 0 {
        return [[]];
    }
    if k < 0 or k > len(xs) {
        return [];
    }
    let out = [];
    let i = 0;
    while i < len(xs) {
        let head = xs[i];
        let rest = xs[0:i] + xs[(i + 1):];
        for sub in permutations(rest, k - 1) {
            out = append(out, [head] + sub);
        }
        i = i + 1;
    }
    return out;
}

fn groupby_key(xs: List, key_fn: Function) -> List {
    if len(xs) == 0 {
        return [];
    }
    let out = [];
    let cur_key = key_fn(xs[0]);
    let cur_grp = [xs[0]];
    let i = 1;
    while i < len(xs) {
        let k = key_fn(xs[i]);
        if k == cur_key {
            cur_grp = append(cur_grp, xs[i]);
        } else {
            out = append(out, [cur_key, cur_grp]);
            cur_key = k;
            cur_grp = [xs[i]];
        }
        i = i + 1;
    }
    out = append(out, [cur_key, cur_grp]);
    return out;
}
