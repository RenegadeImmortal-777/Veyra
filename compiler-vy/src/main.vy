# Veyra Self-Hosted Compiler (Veyra 12 — compiler-vy)
# 100% Pure-Veyra tokenizer, recursive-descent parser, and `veyra-bc-v1` (.vybc)
# bytecode generator capable of compiling its own source code to a 3-stage fixpoint.

let TOKENS = [];
let POS = 0;
let NEXT_TEMP = 0;

fn bc_null() -> Map {
    return {"t": "null"};
}

fn bc_bool(b: Bool) -> Map {
    return {"t": "bool", "v": b};
}

fn bc_num(n: Number) -> Map {
    return {"t": "num", "v": n};
}

fn bc_str(s: String) -> Map {
    return {"t": "str", "v": s};
}

fn op_const_null() -> Map {
    return {"op": "Const", "val": bc_null()};
}

fn op_const_bool(b: Bool) -> Map {
    return {"op": "Const", "val": bc_bool(b)};
}

fn op_const_num(n: Number) -> Map {
    return {"op": "Const", "val": bc_num(n)};
}

fn op_const_str(s: String) -> Map {
    return {"op": "Const", "val": bc_str(s)};
}

fn op_load(name: String) -> Map {
    return {"name": name, "op": "Load"};
}

fn op_define(name: String, is_const: Bool, ty: Any) -> Map {
    return {"const": is_const, "name": name, "op": "Define", "type": ty};
}

fn op_store(name: String) -> Map {
    return {"name": name, "op": "Store"};
}

fn op_call(name: String, argc: Int) -> Map {
    return {"argc": argc, "name": name, "op": "Call"};
}

fn op_jump(target: Int) -> Map {
    return {"op": "Jump", "target": target};
}

fn op_jump_if_false(target: Int) -> Map {
    return {"op": "JumpIfFalse", "target": target};
}

fn op_tag(tag: String) -> Map {
    return {"op": tag};
}

fn is_const_num(op_obj: Map) -> Bool {
    if op_obj["op"] != "Const" {
        return false;
    }
    return op_obj["val"]["t"] == "num";
}

fn fold_binary_tail(ops: List, tag: String) -> List {
    let n = len(ops);
    if n >= 2 {
        let a = ops[n - 2];
        let b = ops[n - 1];
        if is_const_num(a) and is_const_num(b) {
            let av = a["val"]["v"];
            let bv = b["val"]["v"];
            if tag == "Add" {
                return append(slice(ops, 0, n - 2), op_const_num(av + bv));
            }
            if tag == "Sub" {
                return append(slice(ops, 0, n - 2), op_const_num(av - bv));
            }
            if tag == "Mul" {
                return append(slice(ops, 0, n - 2), op_const_num(av * bv));
            }
            if tag == "Div" and bv != 0 {
                return append(slice(ops, 0, n - 2), op_const_num(av / bv));
            }
            if tag == "Mod" and bv != 0 {
                return append(slice(ops, 0, n - 2), op_const_num(av % bv));
            }
        }
    }
    return append(ops, op_tag(tag));
}

fn fold_neg_tail(ops: List) -> List {
    let n = len(ops);
    if n >= 1 {
        let a = ops[n - 1];
        if is_const_num(a) {
            return append(slice(ops, 0, n - 1), op_const_num(0 - a["val"]["v"]));
        }
    }
    return append(ops, op_tag("Neg"));
}

fn unescape_str(raw_tok: String) -> String {
    let inner = slice(raw_tok, 1, len(raw_tok) - 1);
    let chunks = regex_findall("\\\\.|[^\\\\]+", inner);
    let out = [];
    for c in chunks {
        if c == "\\n" {
            out = append(out, "\n");
        } else if c == "\\r" {
            out = append(out, "\r");
        } else if c == "\\t" {
            out = append(out, "\t");
        } else if c == "\\\"" {
            out = append(out, "\"");
        } else if c == "\\'" {
            out = append(out, "'");
        } else if c == "\\\\" {
            out = append(out, "\\");
        } else {
            out = append(out, c);
        }
    }
    return join(out, "");
}

fn tokenize(src: String) -> List {
    let pattern = "#[^\\n]*|\"(?:\\\\.|[^\"\\\\])*\"|'(?:\\\\.|[^'\\\\])*'|->|==|!=|<=|>=|\\+=|-=|\\*=|/=|\\*\\*|[0-9]+(?:\\.[0-9]+)?|[A-Za-z_][A-Za-z0-9_]*|[()\\[\\]{},:;+\\-*/%<>=\\.]";
    let raw = regex_findall(pattern, src);
    let tokens = [];
    for t in raw {
        if not starts_with(t, "#") {
            tokens = append(tokens, t);
        }
    }
    return append(tokens, "<EOF>");
}

fn peek() -> String {
    return TOKENS[POS];
}

fn peek_next() -> String {
    if POS + 1 < len(TOKENS) {
        return TOKENS[POS + 1];
    }
    return "<EOF>";
}

fn advance() -> String {
    let t = TOKENS[POS];
    POS += 1;
    return t;
}

fn expect(want: String) -> String {
    let t = advance();
    assert(t == want);
    return t;
}

fn next_temp(prefix: String) -> String {
    let id = NEXT_TEMP;
    NEXT_TEMP += 1;
    return "__" + prefix + "_" + str(id);
}

fn emit_expr(ops: List) -> List {
    return emit_or(ops);
}

fn emit_or(ops: List) -> List {
    ops = emit_and(ops);
    while peek() == "or" {
        advance();
        let jf = len(ops);
        ops = append(ops, op_jump_if_false(0));
        ops = append(ops, op_const_bool(true));
        let jend = len(ops);
        ops = append(ops, op_jump(0));
        ops[jf] = op_jump_if_false(len(ops));
        ops = emit_and(ops);
        ops = append(ops, op_call("bool", 1));
        ops[jend] = op_jump(len(ops));
    }
    return ops;
}

fn emit_and(ops: List) -> List {
    ops = emit_eq(ops);
    while peek() == "and" {
        advance();
        let jf = len(ops);
        ops = append(ops, op_jump_if_false(0));
        ops = emit_eq(ops);
        ops = append(ops, op_call("bool", 1));
        let jend = len(ops);
        ops = append(ops, op_jump(0));
        ops[jf] = op_jump_if_false(len(ops));
        ops = append(ops, op_const_bool(false));
        ops[jend] = op_jump(len(ops));
    }
    return ops;
}

fn emit_eq(ops: List) -> List {
    ops = emit_add(ops);
    while peek() == "==" or peek() == "!=" or peek() == "<" or peek() == "<=" or peek() == ">" or peek() == ">=" {
        let op = advance();
        ops = emit_add(ops);
        if op == "==" {
            ops = append(ops, op_tag("Eq"));
        } else if op == "!=" {
            ops = append(ops, op_tag("Ne"));
        } else if op == "<" {
            ops = append(ops, op_tag("Lt"));
        } else if op == "<=" {
            ops = append(ops, op_tag("Le"));
        } else if op == ">" {
            ops = append(ops, op_tag("Gt"));
        } else {
            ops = append(ops, op_tag("Ge"));
        }
    }
    return ops;
}

fn emit_add(ops: List) -> List {
    ops = emit_mul(ops);
    while peek() == "+" or peek() == "-" {
        let op = advance();
        ops = emit_mul(ops);
        if op == "+" {
            ops = fold_binary_tail(ops, "Add");
        } else {
            ops = fold_binary_tail(ops, "Sub");
        }
    }
    return ops;
}

fn emit_mul(ops: List) -> List {
    ops = emit_unary(ops);
    while peek() == "*" or peek() == "/" or peek() == "%" {
        let op = advance();
        ops = emit_unary(ops);
        if op == "*" {
            ops = fold_binary_tail(ops, "Mul");
        } else if op == "/" {
            ops = fold_binary_tail(ops, "Div");
        } else {
            ops = fold_binary_tail(ops, "Mod");
        }
    }
    return ops;
}

fn emit_unary(ops: List) -> List {
    if peek() == "not" {
        advance();
        ops = emit_unary(ops);
        return append(ops, op_tag("Not"));
    }
    if peek() == "-" {
        advance();
        ops = emit_unary(ops);
        return fold_neg_tail(ops);
    }
    return emit_postfix(ops);
}

fn emit_postfix(ops: List) -> List {
    ops = emit_primary(ops);
    while peek() == "[" {
        advance();
        ops = emit_expr(ops);
        expect("]");
        ops = append(ops, op_tag("Index"));
    }
    return ops;
}

fn emit_primary(ops: List) -> List {
    if peek() == "(" {
        advance();
        ops = emit_expr(ops);
        expect(")");
        return ops;
    }
    if peek() == "[" {
        advance();
        let count = 0;
        if peek() != "]" {
            ops = emit_expr(ops);
            count += 1;
            while peek() == "," {
                advance();
                ops = emit_expr(ops);
                count += 1;
            }
        }
        expect("]");
        return append(ops, op_call("__list", count));
    }
    if peek() == "{" {
        advance();
        let count = 0;
        if peek() != "}" {
            ops = emit_expr(ops);
            expect(":");
            ops = emit_expr(ops);
            count += 2;
            while peek() == "," {
                advance();
                ops = emit_expr(ops);
                expect(":");
                ops = emit_expr(ops);
                count += 2;
            }
        }
        expect("}");
        return append(ops, op_call("__map", count));
    }
    let t = advance();
    if t == "true" {
        return append(ops, op_const_bool(true));
    }
    if t == "false" {
        return append(ops, op_const_bool(false));
    }
    if t == "null" {
        return append(ops, op_const_null());
    }
    if starts_with(t, "\"") or starts_with(t, "'") {
        return append(ops, op_const_str(unescape_str(t)));
    }
    if len(regex_findall("^[0-9]+(?:\\.[0-9]+)?$", t)) == 1 {
        if contains(t, ".") {
            return append(ops, op_const_num(float(t)));
        }
        return append(ops, op_const_num(int(t)));
    }
    if peek() == "(" {
        advance();
        let argc = 0;
        if peek() != ")" {
            ops = emit_expr(ops);
            argc += 1;
            while peek() == "," {
                advance();
                ops = emit_expr(ops);
                argc += 1;
            }
        }
        expect(")");
        return append(ops, op_call(t, argc));
    }
    return append(ops, op_load(t));
}

fn emit_block(ops: List) -> List {
    expect("{");
    while peek() != "}" and peek() != "<EOF>" {
        ops = emit_stmt(ops);
    }
    expect("}");
    return ops;
}

fn emit_stmt(ops: List) -> List {
    let tok = peek();
    if tok == "let" or tok == "const" {
        let kw = advance();
        let is_const = (kw == "const");
        let name = advance();
        let ty = null;
        if peek() == ":" {
            advance();
            ty = advance();
        }
        expect("=");
        ops = emit_expr(ops);
        expect(";");
        return append(ops, op_define(name, is_const, ty));
    }
    if tok == "return" {
        advance();
        if peek() == ";" {
            advance();
            ops = append(ops, op_const_null());
            return append(ops, op_tag("Return"));
        }
        ops = emit_expr(ops);
        expect(";");
        return append(ops, op_tag("Return"));
    }
    if tok == "if" {
        advance();
        ops = emit_expr(ops);
        let jf = len(ops);
        ops = append(ops, op_jump_if_false(0));
        ops = emit_block(ops);
        if peek() == "else" {
            advance();
            let jend = len(ops);
            ops = append(ops, op_jump(0));
            ops[jf] = op_jump_if_false(len(ops));
            if peek() == "if" {
                ops = emit_stmt(ops);
            } else {
                ops = emit_block(ops);
            }
            ops[jend] = op_jump(len(ops));
        } else {
            ops[jf] = op_jump_if_false(len(ops));
        }
        return ops;
    }
    if tok == "while" {
        advance();
        let start = len(ops);
        ops = emit_expr(ops);
        let jf = len(ops);
        ops = append(ops, op_jump_if_false(0));
        ops = emit_block(ops);
        ops = append(ops, op_jump(start));
        ops[jf] = op_jump_if_false(len(ops));
        return ops;
    }
    if tok == "for" {
        advance();
        let var_name = advance();
        expect("in");
        let iter_name = next_temp("for_iter");
        let idx_name = next_temp("for_idx");
        ops = emit_expr(ops);
        ops = append(ops, op_call("__iter_list", 1));
        ops = append(ops, op_define(iter_name, false, null));
        ops = append(ops, op_const_num(0));
        ops = append(ops, op_define(idx_name, false, null));
        let loop_start = len(ops);
        ops = append(ops, op_load(idx_name));
        ops = append(ops, op_load(iter_name));
        ops = append(ops, op_call("len", 1));
        ops = append(ops, op_tag("Lt"));
        let jf = len(ops);
        ops = append(ops, op_jump_if_false(0));
        ops = append(ops, op_load(iter_name));
        ops = append(ops, op_load(idx_name));
        ops = append(ops, op_tag("Index"));
        ops = append(ops, op_define(var_name, false, null));
        ops = emit_block(ops);
        ops = append(ops, op_load(idx_name));
        ops = append(ops, op_const_num(1));
        ops = append(ops, op_tag("Add"));
        ops = append(ops, op_store(idx_name));
        ops = append(ops, op_jump(loop_start));
        ops[jf] = op_jump_if_false(len(ops));
        return ops;
    }
    if tok == "fn" {
        advance();
        let fn_name = advance();
        expect("(");
        let params = [];
        if peek() != ")" {
            let pname = advance();
            let ptype = null;
            if peek() == ":" {
                advance();
                ptype = advance();
            }
            params = append(params, [pname, ptype]);
            while peek() == "," {
                advance();
                let pn = advance();
                let pt = null;
                if peek() == ":" {
                    advance();
                    pt = advance();
                }
                params = append(params, [pn, pt]);
            }
        }
        expect(")");
        let ret_type = null;
        if peek() == "->" {
            advance();
            ret_type = advance();
        }
        let body = emit_block([]);
        body = append(body, op_const_null());
        body = append(body, op_tag("Return"));
        return append(ops, {
            "async": false,
            "body": body,
            "name": fn_name,
            "op": "Def",
            "params": params,
            "ret": ret_type
        });
    }
    let nx = peek_next();
    if nx == "=" {
        let name = advance();
        expect("=");
        ops = emit_expr(ops);
        expect(";");
        return append(ops, op_store(name));
    }
    if nx == "+=" or nx == "-=" or nx == "*=" or nx == "/=" {
        let name = advance();
        let cop = advance();
        ops = append(ops, op_load(name));
        ops = emit_expr(ops);
        expect(";");
        if cop == "+=" {
            ops = append(ops, op_tag("Add"));
        } else if cop == "-=" {
            ops = append(ops, op_tag("Sub"));
        } else if cop == "*=" {
            ops = append(ops, op_tag("Mul"));
        } else {
            ops = append(ops, op_tag("Div"));
        }
        return append(ops, op_store(name));
    }
    if nx == "[" {
        let name = advance();
        expect("[");
        ops = append(ops, op_load(name));
        ops = emit_expr(ops);
        expect("]");
        expect("=");
        ops = emit_expr(ops);
        expect(";");
        ops = append(ops, op_tag("SetIndex"));
        return append(ops, op_store(name));
    }
    ops = emit_expr(ops);
    expect(";");
    return append(ops, op_tag("Pop"));
}

fn compile_veyra_to_vybc(src: String) -> String {
    TOKENS = tokenize(src);
    POS = 0;
    NEXT_TEMP = 0;
    let ops = [];
    while peek() != "<EOF>" {
        ops = emit_stmt(ops);
    }
    ops = append(ops, op_tag("Halt"));
    let ops_json = json_stringify(ops);
    let digest = sha256(ops_json);
    let doc = {
        "format": "veyra-bc-v1",
        "ops": ops,
        "sha256": digest
    };
    return json_stringify(doc);
}

fn compile_veyra_subset_to_vybc(src: String) -> String {
    return compile_veyra_to_vybc(src);
}

fn bootstrap_compile_self(compiler_src: String) -> String {
    return compile_veyra_to_vybc(compiler_src);
}
