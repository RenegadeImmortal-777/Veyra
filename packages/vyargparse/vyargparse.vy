fn spec(program: String, description: String) -> Map {
    return {
        "program": program,
        "description": description,
        "flags": {},
        "options": {},
        "short_map": {}
    };
}

fn add_flag(parser: Map, name: String, short_name: String, help: String) -> Map {
    let flags = parser["flags"];
    flags[name] = {"short": short_name, "help": help, "default": false};
    parser["flags"] = flags;
    if short_name != "" {
        let sm = parser["short_map"];
        sm[short_name] = name;
        parser["short_map"] = sm;
    }
    return parser;
}

fn add_option(parser: Map, name: String, short_name: String, default_val: Any, help: String) -> Map {
    let opts = parser["options"];
    opts[name] = {"short": short_name, "default": default_val, "help": help};
    parser["options"] = opts;
    if short_name != "" {
        let sm = parser["short_map"];
        sm[short_name] = name;
        parser["short_map"] = sm;
    }
    return parser;
}

fn parse(parser: Map, argv: List) -> Map {
    let values = {};
    for k in keys(parser["flags"]) {
        values[k] = false;
    }
    for k in keys(parser["options"]) {
        values[k] = parser["options"][k]["default"];
    }
    let positionals = [];
    let i = 0;
    while i < len(argv) {
        let tok = str(argv[i]);
        if tok == "--help" or tok == "-h" {
            values["help"] = true;
            i = i + 1;
        } else if len(tok) > 2 and tok[0] == "-" and tok[1] == "-" {
            let body = replace(tok, "--", "");
            if contains(body, "=") {
                let kv = split(body, "=");
                values[kv[0]] = kv[1];
            } else if contains(parser["flags"], body) {
                values[body] = true;
            } else if contains(parser["options"], body) and i + 1 < len(argv) {
                i = i + 1;
                values[body] = argv[i];
            } else {
                values[body] = true;
            }
            i = i + 1;
        } else if len(tok) == 2 and tok[0] == "-" {
            let short_ch = tok[1];
            if contains(parser["short_map"], short_ch) {
                let target = parser["short_map"][short_ch];
                if contains(parser["flags"], target) {
                    values[target] = true;
                } else if contains(parser["options"], target) and i + 1 < len(argv) {
                    i = i + 1;
                    values[target] = argv[i];
                }
            }
            i = i + 1;
        } else {
            positionals = append(positionals, tok);
            i = i + 1;
        }
    }
    return {"values": values, "positionals": positionals};
}

fn format_help(parser: Map) -> String {
    let lines = [parser["program"] + " - " + parser["description"], "", "Options:"];
    for k in keys(parser["flags"]) {
        let f = parser["flags"][k];
        lines = append(lines, "  --" + k + " (-" + f["short"] + ")  " + f["help"]);
    }
    for k in keys(parser["options"]) {
        let o = parser["options"][k];
        lines = append(lines, "  --" + k + " <val>  " + o["help"]);
    }
    return join(lines, "\n");
}
