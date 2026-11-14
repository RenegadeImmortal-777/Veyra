fn create(max_size: Number) -> Map {
    return {
        "max_size": max_size,
        "entries": {},
        "order": [],
        "hits": 0,
        "misses": 0
    };
}

fn has(cache: Map, key: String) -> Bool {
    return contains(cache["entries"], key);
}

fn touch(cache: Map, key: String) -> Map {
    if contains(cache["entries"], key) {
        cache["hits"] = cache["hits"] + 1;
        let order = cache["order"];
        let next_order = [];
        for k in order {
            if k != key {
                next_order = append(next_order, k);
            }
        }
        next_order = append(next_order, key);
        cache["order"] = next_order;
        return cache;
    }
    cache["misses"] = cache["misses"] + 1;
    return cache;
}

fn get(cache: Map, key: String, default_val: Any) -> Any {
    if contains(cache["entries"], key) {
        return cache["entries"][key];
    }
    return default_val;
}

fn put(cache: Map, key: String, value: Any) -> Map {
    let entries = cache["entries"];
    let max_size = cache["max_size"];
    let order = cache["order"];
    let next_order = [];
    for k in order {
        if k != key {
            next_order = append(next_order, k);
        }
    }
    if len(next_order) >= max_size and max_size > 0 {
        let oldest = next_order[0];
        let trimmed = [];
        for i in range(1, len(next_order)) {
            trimmed = append(trimmed, next_order[i]);
        }
        next_order = trimmed;
        let next_entries = {};
        for ek in keys(entries) {
            if ek != oldest {
                next_entries[ek] = entries[ek];
            }
        }
        entries = next_entries;
    }
    entries[key] = value;
    next_order = append(next_order, key);
    cache["entries"] = entries;
    cache["order"] = next_order;
    return cache;
}

fn stats(cache: Map) -> Map {
    return {
        "size": len(cache["order"]),
        "max_size": cache["max_size"],
        "hits": cache["hits"],
        "misses": cache["misses"]
    };
}

fn clear(cache: Map) -> Map {
    cache["entries"] = {};
    cache["order"] = [];
    return cache;
}
