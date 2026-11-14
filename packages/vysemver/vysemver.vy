fn _is_digits(s: String) -> Bool {
    if len(s) == 0 {
        return false;
    }
    let i = 0;
    while i < len(s) {
        let ch = s[i];
        if ch < "0" or ch > "9" {
            return false;
        }
        i = i + 1;
    }
    return true;
}

fn parse(ver: String) -> Map {
    let s = trim(ver);
    if len(s) > 0 and (s[0] == "v" or s[0] == "V") {
        s = s[1:];
    }
    let build = "";
    let plus_parts = split(s, "+");
    if len(plus_parts) > 1 {
        s = plus_parts[0];
        build = plus_parts[1];
    }
    let pre = "";
    let dash_idx = -1;
    let i = 0;
    while i < len(s) {
        if s[i] == "-" {
            dash_idx = i;
            break;
        }
        i = i + 1;
    }
    if dash_idx >= 0 {
        pre = s[(dash_idx + 1):];
        s = s[0:dash_idx];
    }
    let nums = split(s, ".");
    let maj = int(float(nums[0])) if len(nums) > 0 and nums[0] != "" else 0;
    let min_v = int(float(nums[1])) if len(nums) > 1 and nums[1] != "" else 0;
    let pat = int(float(nums[2])) if len(nums) > 2 and nums[2] != "" else 0;
    return {
        "major": maj,
        "minor": min_v,
        "patch": pat,
        "pre": pre,
        "build": build
    };
}

fn format(v: Map) -> String {
    let s = str(v["major"]) + "." + str(v["minor"]) + "." + str(v["patch"]);
    if contains(v, "pre") and v["pre"] != "" {
        s = s + "-" + v["pre"];
    }
    if contains(v, "build") and v["build"] != "" {
        s = s + "+" + v["build"];
    }
    return s;
}

fn _cmp_pre(pa: String, pb: String) -> Number {
    if pa == pb {
        return 0;
    }
    if pa == "" {
        return 1;
    }
    if pb == "" {
        return -1;
    }
    let la = split(pa, ".");
    let lb = split(pb, ".");
    let n = len(la) if len(la) < len(lb) else len(lb);
    let i = 0;
    while i < n {
        let a = la[i];
        let b = lb[i];
        if a != b {
            let da = _is_digits(a);
            let db = _is_digits(b);
            if da and db {
                let na = float(a);
                let nb = float(b);
                return -1 if na < nb else 1;
            }
            if da and (not db) {
                return -1;
            }
            if (not da) and db {
                return 1;
            }
            return -1 if a < b else 1;
        }
        i = i + 1;
    }
    if len(la) < len(lb) {
        return -1;
    }
    return 1;
}

fn compare(ver_a: String, ver_b: String) -> Number {
    let a = parse(ver_a);
    let b = parse(ver_b);
    if a["major"] != b["major"] {
        return -1 if a["major"] < b["major"] else 1;
    }
    if a["minor"] != b["minor"] {
        return -1 if a["minor"] < b["minor"] else 1;
    }
    if a["patch"] != b["patch"] {
        return -1 if a["patch"] < b["patch"] else 1;
    }
    return _cmp_pre(a["pre"], b["pre"]);
}

fn _cmp_core(a: Map, b: Map) -> Number {
    if a["major"] != b["major"] {
        return -1 if a["major"] < b["major"] else 1;
    }
    if a["minor"] != b["minor"] {
        return -1 if a["minor"] < b["minor"] else 1;
    }
    if a["patch"] != b["patch"] {
        return -1 if a["patch"] < b["patch"] else 1;
    }
    return 0;
}

fn _check_clause(ver: String, clause: String) -> Bool {
    let c = trim(clause);
    if c == "" or c == "*" {
        return true;
    }
    let v = parse(ver);
    if starts_with(c, "^") {
        let r = parse(c[1:]);
        if compare(ver, c[1:]) < 0 {
            return false;
        }
        if r["major"] > 0 {
            return v["major"] == r["major"];
        }
        if r["minor"] > 0 {
            return v["major"] == 0 and v["minor"] == r["minor"];
        }
        return v["major"] == 0 and v["minor"] == 0 and v["patch"] == r["patch"];
    }
    if starts_with(c, "~") {
        let r = parse(c[1:]);
        return v["major"] == r["major"] and v["minor"] == r["minor"] and v["patch"] >= r["patch"] and compare(ver, c[1:]) >= 0;
    }
    if starts_with(c, ">=") {
        return compare(ver, c[2:]) >= 0;
    }
    if starts_with(c, "<=") {
        return compare(ver, c[2:]) <= 0;
    }
    if starts_with(c, ">") {
        return compare(ver, c[1:]) > 0;
    }
    if starts_with(c, "<") {
        return compare(ver, c[1:]) < 0;
    }
    if starts_with(c, "=") {
        return compare(ver, c[1:]) == 0;
    }
    return compare(ver, c) == 0;
}

fn satisfies(ver: String, constraint: String) -> Bool {
    let norm = replace(trim(constraint), ",", " ");
    let parts = split(norm, " ");
    for part in parts {
        let p = trim(part);
        if p != "" {
            if not _check_clause(ver, p) {
                return false;
            }
        }
    }
    return true;
}

fn bump_major(ver: String) -> String {
    let v = parse(ver);
    return format({"major": v["major"] + 1, "minor": 0, "patch": 0, "pre": "", "build": ""});
}

fn bump_minor(ver: String) -> String {
    let v = parse(ver);
    return format({"major": v["major"], "minor": v["minor"] + 1, "patch": 0, "pre": "", "build": ""});
}

fn bump_patch(ver: String) -> String {
    let v = parse(ver);
    return format({"major": v["major"], "minor": v["minor"], "patch": v["patch"] + 1, "pre": "", "build": ""});
}

fn max_satisfying(versions: List, constraint: String) -> String {
    let best = "";
    for ver in versions {
        if satisfies(ver, constraint) {
            if best == "" or compare(ver, best) > 0 {
                best = ver;
            }
        }
    }
    return best;
}
