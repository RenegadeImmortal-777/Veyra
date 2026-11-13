fn of(scaled: Number, scale: Number) -> Map {
    return {"scaled": round(scaled), "scale": scale};
}

fn from_str(text: String, scale: Number) -> Map {
    let s = trim(text);
    let neg = false;
    let body = s;
    if starts_with(body, "-") {
        neg = true;
        body = slice(body, 1, len(body));
    } else if starts_with(body, "+") {
        body = slice(body, 1, len(body));
    }
    let parts = split(body, ".");
    let whole = 0;
    if len(parts) > 0 and parts[0] != "" {
        whole = int(parts[0]);
    }
    let frac_str = "";
    if len(parts) > 1 {
        frac_str = parts[1];
    }
    while len(frac_str) < scale {
        frac_str = frac_str + "0";
    }
    if len(frac_str) > scale {
        frac_str = slice(frac_str, 0, scale);
    }
    let frac = 0;
    if frac_str != "" {
        frac = int(frac_str);
    }
    let factor = pow(10, scale);
    let mag = whole * factor + frac;
    if neg {
        mag = -mag;
    }
    return {"scaled": round(mag), "scale": scale};
}

fn add(a: Map, b: Map) -> Map {
    let sa = a["scale"];
    let sb = b["scale"];
    if sa == sb {
        return of(a["scaled"] + b["scaled"], sa);
    }
    let s = sb;
    if sa > sb {
        s = sa;
    }
    let va = a["scaled"] * pow(10, s - sa);
    let vb = b["scaled"] * pow(10, s - sb);
    return of(va + vb, s);
}

fn sub(a: Map, b: Map) -> Map {
    return add(a, of(-b["scaled"], b["scale"]));
}

fn mul(a: Map, b: Map) -> Map {
    let sa = a["scale"];
    let sb = b["scale"];
    let s = sb;
    if sa > sb {
        s = sa;
    }
    let denom = pow(10, sa + sb - s);
    return of(round((a["scaled"] * b["scaled"]) / denom), s);
}

fn format(d: Map) -> String {
    let scale = d["scale"];
    let raw = round(d["scaled"]);
    if scale <= 0 {
        return str(int(raw));
    }
    let sign = "";
    if raw < 0 {
        sign = "-";
    }
    let mag = abs(raw);
    let factor = pow(10, scale);
    let whole = floor(mag / factor);
    let frac = str(int(mag - whole * factor));
    while len(frac) < scale {
        frac = "0" + frac;
    }
    return sign + str(int(whole)) + "." + frac;
}

fn to_number(d: Map) -> Number {
    return d["scaled"] / pow(10, d["scale"]);
}
