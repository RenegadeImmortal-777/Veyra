fn optional(inner: Any) -> Map {
    return {"kind": "optional", "inner": inner};
}

fn union_of(options: List) -> Map {
    return {"kind": "union", "options": options};
}

fn literal_of(values: List) -> Map {
    return {"kind": "literal", "values": values};
}

fn list_of(elem: Any) -> Map {
    return {"kind": "list", "elem": elem};
}

fn dict_of(key_spec: Any, val_spec: Any) -> Map {
    return {"kind": "dict", "key": key_spec, "val": val_spec};
}

fn typed_dict(fields: Map) -> Map {
    return {"kind": "typed_dict", "fields": fields};
}

fn _is_int_num(v: Any) -> Bool {
    let tv = type_of(v);
    if tv == "Int" {
        return true;
    }
    if tv != "Number" {
        return false;
    }
    return int(v) == v;
}

fn matches(val: Any, spec: Any) -> Bool {
    let t = type_of(spec);
    if t == "String" {
        if spec == "any" or spec == "Any" {
            return true;
        }
        if spec == "int" or spec == "Int" {
            return _is_int_num(val);
        }
        if spec == "float" or spec == "num" or spec == "Number" or spec == "Float" {
            let tv = type_of(val);
            return tv == "Number" or tv == "Int";
        }
        if spec == "str" or spec == "String" {
            return type_of(val) == "String";
        }
        if spec == "bool" or spec == "Bool" {
            return type_of(val) == "Bool";
        }
        if spec == "null" or spec == "Null" or spec == "None" {
            return val == null;
        }
        if spec == "list" or spec == "List" {
            return type_of(val) == "List";
        }
        if spec == "map" or spec == "dict" or spec == "Map" {
            return type_of(val) == "Map";
        }
        if spec == "fn" or spec == "function" or spec == "Function" or spec == "Callable" {
            return type_of(val) == "Function";
        }
        return isinstance(val, spec);
    }
    if t == "Map" and contains(spec, "kind") {
        let kind = spec["kind"];
        if kind == "optional" {
            if val == null {
                return true;
            }
            return matches(val, spec["inner"]);
        }
        if kind == "union" {
            for opt in spec["options"] {
                if matches(val, opt) {
                    return true;
                }
            }
            return false;
        }
        if kind == "literal" {
            for lit in spec["values"] {
                if val == lit {
                    return true;
                }
            }
            return false;
        }
        if kind == "list" {
            if type_of(val) != "List" {
                return false;
            }
            for item in val {
                if not matches(item, spec["elem"]) {
                    return false;
                }
            }
            return true;
        }
        if kind == "dict" {
            if type_of(val) != "Map" {
                return false;
            }
            for k in keys(val) {
                if not matches(k, spec["key"]) or not matches(val[k], spec["val"]) {
                    return false;
                }
            }
            return true;
        }
        if kind == "typed_dict" {
            if type_of(val) != "Map" {
                return false;
            }
            let fields = spec["fields"];
            for k in keys(fields) {
                let fspec = fields[k];
                let is_opt = type_of(fspec) == "Map" and contains(fspec, "kind") and fspec["kind"] == "optional";
                if not contains(val, k) {
                    if not is_opt {
                        return false;
                    }
                } else {
                    if not matches(val[k], fspec) {
                        return false;
                    }
                }
            }
            return true;
        }
    }
    if t == "Object" {
        return isinstance(val, spec);
    }
    if t == "List" {
        for opt in spec {
            if matches(val, opt) {
                return true;
            }
        }
        return false;
    }
    return false;
}

fn describe_spec(spec: Any) -> String {
    if type_of(spec) == "String" {
        return spec;
    }
    if type_of(spec) == "Map" and contains(spec, "kind") {
        let kind = spec["kind"];
        if kind == "optional" {
            return "Optional[" + describe_spec(spec["inner"]) + "]";
        }
        if kind == "list" {
            return "List[" + describe_spec(spec["elem"]) + "]";
        }
        if kind == "dict" {
            return "Dict[" + describe_spec(spec["key"]) + ", " + describe_spec(spec["val"]) + "]";
        }
        return kind;
    }
    return str(spec);
}

fn check_value(val: Any, spec: Any, label: String) -> Any {
    if not matches(val, spec) {
        throw "TypeError: " + label + " expected " + describe_spec(spec) + ", got " + type_of(val);
    }
    return val;
}

fn assert_schema(val: Any, schema: Map) -> Bool {
    if not matches(val, schema) {
        throw "SchemaError: value failed schema validation for " + describe_spec(schema);
    }
    return true;
}
