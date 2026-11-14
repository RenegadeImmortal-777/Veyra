fn diff_lines(a_text: String, b_text: String) -> List {
    let a = split(a_text, "\n");
    let b = split(b_text, "\n");
    let m = len(a);
    let n = len(b);
    let ops = [];
    let i = 0;
    let j = 0;
    while i < m and j < n {
        if a[i] == b[j] {
            ops = append(ops, {"op": "=", "line": a[i], "left": i + 1, "right": j + 1});
            i = i + 1;
            j = j + 1;
        } else {
            let found_in_b = -1;
            let look_b_end = j + 8;
            let kb = j + 1;
            while kb < n and kb < look_b_end {
                if a[i] == b[kb] {
                    found_in_b = kb;
                    break;
                }
                kb = kb + 1;
            }
            let found_in_a = -1;
            let look_a_end = i + 8;
            let ka = i + 1;
            while ka < m and ka < look_a_end {
                if a[ka] == b[j] {
                    found_in_a = ka;
                    break;
                }
                ka = ka + 1;
            }
            if found_in_b >= 0 and (found_in_a < 0 or (found_in_b - j) <= (found_in_a - i)) {
                while j < found_in_b {
                    ops = append(ops, {"op": "+", "line": b[j], "right": j + 1});
                    j = j + 1;
                }
            } else if found_in_a >= 0 {
                while i < found_in_a {
                    ops = append(ops, {"op": "-", "line": a[i], "left": i + 1});
                    i = i + 1;
                }
            } else {
                ops = append(ops, {"op": "-", "line": a[i], "left": i + 1});
                ops = append(ops, {"op": "+", "line": b[j], "right": j + 1});
                i = i + 1;
                j = j + 1;
            }
        }
    }
    while i < m {
        ops = append(ops, {"op": "-", "line": a[i], "left": i + 1});
        i = i + 1;
    }
    while j < n {
        ops = append(ops, {"op": "+", "line": b[j], "right": j + 1});
        j = j + 1;
    }
    return ops;
}

fn unified(a_text: String, b_text: String, from_label: String, to_label: String) -> String {
    let ops = diff_lines(a_text, b_text);
    let lines = ["--- " + from_label, "+++ " + to_label];
    for entry in ops {
        let mark = entry["op"];
        if mark == "=" {
            lines = append(lines, " " + entry["line"]);
        } else {
            lines = append(lines, mark + entry["line"]);
        }
    }
    return join(lines, "\n");
}

fn ratio(a_text: String, b_text: String) -> Number {
    let ops = diff_lines(a_text, b_text);
    if len(ops) == 0 {
        return 1.0;
    }
    let same = 0;
    for entry in ops {
        if entry["op"] == "=" {
            same = same + 1;
        }
    }
    return same / len(ops);
}
