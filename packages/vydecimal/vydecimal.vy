fn _is_dec_map(x: Any) -> Bool {
    return type_of(x) == "Map" and contains(x, "value");
}

fn _scale_str(scaled: Any, scale: Number) -> String {
    let s = str(scaled);
    if scale <= 0 {
        return dec(s);
    }
    let denom = "1";
    let i = 0;
    while i < scale {
        denom = denom + "0";
        i = i + 1;
    }
    return dec_quantize(dec_div(s, denom, scale + 4), scale);
}

fn _val_str(d: Map) -> String {
    if contains(d, "value") {
        return str(d["value"]);
    }
    return _scale_str(d["scaled"], d["scale"]);
}

fn _scale_of(d: Map) -> Number {
    if contains(d, "scale") and d["scale"] != null {
        return d["scale"];
    }
    return -1;
}

fn _make(val: String, scale: Number) -> Map {
    let exact = dec(val);
    let shown = dec_quantize(exact, scale) if scale >= 0 else exact;
    let scaled_num = 0;
    if scale >= 0 and scale <= 9 and len(exact) <= 18 {
        let factor = 1;
        let i = 0;
        while i < scale {
            factor = factor * 10;
            i = i + 1;
        }
        scaled_num = round(float(exact) * factor, 0);
    }
    return {"value": shown, "scale": scale, "scaled": scaled_num};
}

fn of(scaled: Any, scale: Number) -> Map {
    return _make(_scale_str(scaled, scale), scale);
}

fn parse(text: String) -> Map {
    return _make(dec(text), -1);
}

fn from_str(text: String, scale: Number) -> Map {
    let exact = dec(text);
    if scale < 0 {
        return _make(exact, -1);
    }
    return _make(dec_quantize(exact, scale), scale);
}

fn add(a: Map, b: Map) -> Map {
    let sa = _scale_of(a);
    let sb = _scale_of(b);
    let out_scale = -1;
    if sa >= 0 and sb >= 0 {
        out_scale = sa if sa >= sb else sb;
    }
    return _make(dec_add(_val_str(a), _val_str(b)), out_scale);
}

fn sub(a: Map, b: Map) -> Map {
    let sa = _scale_of(a);
    let sb = _scale_of(b);
    let out_scale = -1;
    if sa >= 0 and sb >= 0 {
        out_scale = sa if sa >= sb else sb;
    }
    return _make(dec_sub(_val_str(a), _val_str(b)), out_scale);
}

fn mul(a: Map, b: Map) -> Map {
    let sa = _scale_of(a);
    let sb = _scale_of(b);
    let out_scale = -1;
    if sa >= 0 and sb >= 0 {
        out_scale = sa if sa >= sb else sb;
    }
    return _make(dec_mul(_val_str(a), _val_str(b)), out_scale);
}

fn div(a: Map, b: Map, prec: Number) -> Map {
    return _make(dec_div(_val_str(a), _val_str(b), prec), -1);
}

fn sqrt(a: Map, prec: Number) -> Map {
    return _make(dec_sqrt(_val_str(a), prec), -1);
}

fn round_places(a: Map, places: Number) -> Map {
    return _make(dec_round(_val_str(a), places), places if places >= 0 else 0);
}

fn quantize(a: Map, places: Number) -> Map {
    return _make(dec_quantize(_val_str(a), places), places);
}

fn cmp(a: Map, b: Map) -> Number {
    return dec_cmp(_val_str(a), _val_str(b));
}

fn abs_val(a: Map) -> Map {
    return _make(dec_abs(_val_str(a)), _scale_of(a));
}

fn neg(a: Map) -> Map {
    return _make(dec_neg(_val_str(a)), _scale_of(a));
}

fn format(d: Map) -> String {
    let s = _scale_of(d);
    let v = _val_str(d);
    if s >= 0 {
        return dec_quantize(v, s);
    }
    return dec(v);
}

fn to_number(d: Map) -> Number {
    return float(format(d));
}

class Decimal {
    fn init(self, text) {
        self.raw = dec(str(text));
    }

    fn add(self, other) {
        let ov = other.raw if type_of(other) == "Object" else dec(str(other));
        return Decimal(dec_add(self.raw, ov));
    }

    fn sub(self, other) {
        let ov = other.raw if type_of(other) == "Object" else dec(str(other));
        return Decimal(dec_sub(self.raw, ov));
    }

    fn mul(self, other) {
        let ov = other.raw if type_of(other) == "Object" else dec(str(other));
        return Decimal(dec_mul(self.raw, ov));
    }

    fn div(self, other, prec) {
        let ov = other.raw if type_of(other) == "Object" else dec(str(other));
        return Decimal(dec_div(self.raw, ov, prec));
    }

    fn sqrt(self, prec) {
        return Decimal(dec_sqrt(self.raw, prec));
    }

    fn quantize(self, places) {
        return Decimal(dec_quantize(self.raw, places));
    }

    fn round(self, places) {
        return Decimal(dec_round(self.raw, places));
    }

    fn cmp(self, other) {
        let ov = other.raw if type_of(other) == "Object" else dec(str(other));
        return dec_cmp(self.raw, ov);
    }

    fn __add__(self, other) {
        return self.add(other);
    }

    fn __sub__(self, other) {
        return self.sub(other);
    }

    fn __mul__(self, other) {
        return self.mul(other);
    }

    fn __eq__(self, other) {
        if type_of(other) != "Object" {
            return false;
        }
        return dec_cmp(self.raw, other.raw) == 0;
    }

    fn __str__(self) {
        return self.raw;
    }

    fn str(self) {
        return self.raw;
    }
}
