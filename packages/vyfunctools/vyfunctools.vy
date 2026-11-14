fn identity(x: Any) -> Any {
    return x;
}

fn constantly(val: Any) -> Function {
    return fn() {
        return val;
    };
}

fn compose(f: Function, g: Function) -> Function {
    return fn(x) {
        return f(g(x));
    };
}

fn pipe(val: Any, fns: List) -> Any {
    let acc = val;
    for f in fns {
        acc = f(acc);
    }
    return acc;
}

fn reduce(fn_step: Function, xs: List, init: Any) -> Any {
    let acc = init;
    for item in xs {
        acc = fn_step(acc, item);
    }
    return acc;
}

fn partial(fn_target: Function, bound_args: List) -> Function {
    return fn(*rest) {
        let all_args = bound_args + rest;
        return fn_target(*all_args);
    };
}

fn memoize(fn_target: Function) -> Function {
    let cache = {};
    return fn(arg) {
        let key = str(type_of(arg)) + ":" + str(arg);
        if contains(cache, key) {
            return cache[key];
        }
        let res = fn_target(arg);
        cache[key] = res;
        return res;
    };
}

fn lru_cache(maxsize: Number, fn_target: Function) -> Function {
    let cache = {};
    let order = [];
    return fn(arg) {
        let key = str(type_of(arg)) + ":" + str(arg);
        if contains(cache, key) {
            let next_order = [];
            for k in order {
                if k != key {
                    next_order = append(next_order, k);
                }
            }
            order = append(next_order, key);
            return cache[key];
        }
        let res = fn_target(arg);
        if maxsize > 0 {
            if len(order) >= maxsize {
                let evicted = order[0];
                let rest = order[1:];
                let new_cache = {};
                for k in rest {
                    if contains(cache, k) {
                        new_cache[k] = cache[k];
                    }
                }
                cache = new_cache;
                order = rest;
                let _unused = evicted;
            }
            cache[key] = res;
            order = append(order, key);
        }
        return res;
    };
}

fn once(fn_target: Function) -> Function {
    let done = false;
    let saved = null;
    return fn(*args) {
        if not done {
            saved = fn_target(*args);
            done = true;
        }
        return saved;
    };
}
