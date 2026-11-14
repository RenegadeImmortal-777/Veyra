// Lexer: `.vy` source text to `Tok` stream.
//
// Extracted from `main.rs` without behavior change; see docs/ENGINE-SPLIT.md.

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Tok {
    Num(f64),
    Str(String),
    Id(String),
    Let,
    Const,
    Fn,
    Async,
    Class,
    Return,
    Yield,
    Break,
    Continue,
    If,
    Else,
    While,
    For,
    In,
    Try,
    Catch,
    Finally,
    Throw,
    Match,
    Case,
    Await,
    With,
    As,
    And,
    Or,
    Not,
    True,
    False,
    Null,
    Lp,
    Rp,
    Lb,
    Rb,
    Ls,
    Rs,
    Comma,
    At,
    Colon,
    Dot,
    Semi,
    Plus,
    Minus,
    Star,
    StarStar,
    Slash,
    Percent,
    PlusEq,
    MinusEq,
    StarEq,
    SlashEq,
    Eq,
    EqEq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Bang,
    Pipe,
    Arrow,
    Eof,
}

pub(crate) fn lex(src: &str) -> Result<Vec<Tok>, String> {
    Ok(lex_with_lines(src)?.into_iter().map(|(t, _)| t).collect())
}

pub(crate) fn lex_with_lines(src: &str) -> Result<Vec<(Tok, usize)>, String> {
    let mut out = Vec::new();
    let cs: Vec<char> = src.chars().collect();
    let mut i = 0;
    let mut line = 1usize;
    while i < cs.len() {
        let c = cs[i];
        if c.is_whitespace() {
            if c == '\n' {
                line += 1;
            }
            i += 1;
            continue;
        }
        if (c == '/' && i + 1 < cs.len() && cs[i + 1] == '/') || c == '#' {
            i += if c == '#' { 1 } else { 2 };
            while i < cs.len() && cs[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && i + 1 < cs.len() && cs[i + 1] == '*' {
            let start_line = line;
            i += 2;
            while i + 1 < cs.len() && !(cs[i] == '*' && cs[i + 1] == '/') {
                if cs[i] == '\n' {
                    line += 1;
                }
                i += 1;
            }
            if i + 1 >= cs.len() {
                return Err(format!("line {start_line}: unterminated block comment"));
            }
            i += 2;
            continue;
        }
        if c.is_ascii_digit() {
            let s = i;
            i += 1;
            while i < cs.len() && (cs[i].is_ascii_digit() || cs[i] == '.') {
                i += 1;
            }
            let n: f64 = cs[s..i]
                .iter()
                .collect::<String>()
                .parse()
                .map_err(|_| format!("line {line}: bad number"))?;
            out.push((Tok::Num(n), line));
            continue;
        }
        if (c == 'f' || c == 'F') && i + 1 < cs.len() && (cs[i + 1] == '"' || cs[i + 1] == '\'') {
            let start_line = line;
            let quote = cs[i + 1];
            i += 2;
            out.push((Tok::Lp, start_line));
            out.push((Tok::Str(String::new()), start_line));
            let mut lit = String::new();
            while i < cs.len() && cs[i] != quote {
                if cs[i] == '\n' {
                    line += 1;
                }
                if cs[i] == '\\' && i + 1 < cs.len() {
                    i += 1;
                    lit.push(match cs[i] {
                        'n' => '\n',
                        't' => '\t',
                        'r' => '\r',
                        x => x,
                    });
                    i += 1;
                    continue;
                }
                if cs[i] == '{' {
                    if i + 1 < cs.len() && cs[i + 1] == '{' {
                        lit.push('{');
                        i += 2;
                        continue;
                    }
                    if !lit.is_empty() {
                        out.push((Tok::Plus, line));
                        out.push((Tok::Str(std::mem::take(&mut lit)), line));
                    }
                    i += 1;
                    let mut expr_src = String::new();
                    let mut depth = 1usize;
                    while i < cs.len() && depth > 0 {
                        if cs[i] == '{' {
                            depth += 1;
                            expr_src.push(cs[i]);
                        } else if cs[i] == '}' {
                            depth -= 1;
                            if depth > 0 {
                                expr_src.push(cs[i]);
                            }
                        } else {
                            if cs[i] == '\n' {
                                line += 1;
                            }
                            expr_src.push(cs[i]);
                        }
                        i += 1;
                    }
                    if depth != 0 {
                        return Err(format!("line {start_line}: unterminated '{{' in f-string"));
                    }
                    let inner_toks = lex_with_lines(&expr_src)?;
                    out.push((Tok::Plus, line));
                    out.push((Tok::Id("str".into()), line));
                    out.push((Tok::Lp, line));
                    for (t, _) in inner_toks {
                        if !matches!(t, Tok::Eof) {
                            out.push((t, line));
                        }
                    }
                    out.push((Tok::Rp, line));
                    continue;
                }
                if cs[i] == '}' && i + 1 < cs.len() && cs[i + 1] == '}' {
                    lit.push('}');
                    i += 2;
                    continue;
                }
                lit.push(cs[i]);
                i += 1;
            }
            if i >= cs.len() {
                return Err(format!("line {start_line}: unterminated f-string"));
            }
            i += 1;
            if !lit.is_empty() {
                out.push((Tok::Plus, line));
                out.push((Tok::Str(lit), line));
            }
            out.push((Tok::Rp, start_line));
            continue;
        }
        if c == '"' || c == '\'' {
            let start_line = line;
            let quote = c;
            i += 1;
            let mut s = String::new();
            while i < cs.len() && cs[i] != quote {
                if cs[i] == '\n' {
                    line += 1;
                }
                if cs[i] == '\\' && i + 1 < cs.len() {
                    i += 1;
                    s.push(match cs[i] {
                        'n' => '\n',
                        't' => '\t',
                        'r' => '\r',
                        x => x,
                    });
                } else {
                    s.push(cs[i]);
                }
                i += 1;
            }
            if i >= cs.len() {
                return Err(format!("line {start_line}: unterminated string"));
            }
            i += 1;
            out.push((Tok::Str(s), start_line));
            continue;
        }
        if c.is_ascii_alphabetic() || c == '_' {
            let s = i;
            i += 1;
            while i < cs.len() && (cs[i].is_ascii_alphanumeric() || cs[i] == '_') {
                i += 1;
            }
            let w: String = cs[s..i].iter().collect();
            if w == "elif" {
                out.push((Tok::Else, line));
                out.push((Tok::If, line));
                continue;
            }
            let tok = match w.as_str() {
                "let" => Tok::Let,
                "const" => Tok::Const,
                "fn" | "def" => Tok::Fn,
                "async" => Tok::Async,
                "class" => Tok::Class,
                "return" => Tok::Return,
                "yield" => Tok::Yield,
                "break" => Tok::Break,
                "continue" => Tok::Continue,
                "if" => Tok::If,
                "else" => Tok::Else,
                "while" => Tok::While,
                "for" => Tok::For,
                "in" => Tok::In,
                "try" => Tok::Try,
                "catch" | "except" => Tok::Catch,
                "finally" => Tok::Finally,
                "throw" | "raise" => Tok::Throw,
                "match" => Tok::Match,
                "case" => Tok::Case,
                "await" => Tok::Await,
                "with" => Tok::With,
                "as" => Tok::As,
                "and" => Tok::And,
                "or" => Tok::Or,
                "not" => Tok::Not,
                "true" | "True" => Tok::True,
                "false" | "False" => Tok::False,
                "null" | "None" => Tok::Null,
                _ => Tok::Id(w),
            };
            out.push((tok, line));
            continue;
        }
        macro_rules! two {
            ($ch:expr,$eq:expr,$one:expr) => {{
                if i + 1 < cs.len() && cs[i + 1] == '=' {
                    i += 2;
                    out.push(($eq, line));
                } else {
                    i += 1;
                    out.push(($one, line));
                }
                continue;
            }};
        }
        match c {
            '(' => {
                out.push((Tok::Lp, line));
                i += 1
            }
            ')' => {
                out.push((Tok::Rp, line));
                i += 1
            }
            '{' => {
                out.push((Tok::Lb, line));
                i += 1
            }
            '}' => {
                out.push((Tok::Rb, line));
                i += 1
            }
            '[' => {
                out.push((Tok::Ls, line));
                i += 1
            }
            ']' => {
                out.push((Tok::Rs, line));
                i += 1
            }
            ',' => {
                out.push((Tok::Comma, line));
                i += 1
            }
            '@' => {
                out.push((Tok::At, line));
                i += 1
            }
            ':' => {
                out.push((Tok::Colon, line));
                i += 1
            }
            '.' => {
                out.push((Tok::Dot, line));
                i += 1
            }
            ';' => {
                out.push((Tok::Semi, line));
                i += 1
            }
            '+' => two!('=', Tok::PlusEq, Tok::Plus),
            '-' => {
                if i + 1 < cs.len() && cs[i + 1] == '>' {
                    out.push((Tok::Arrow, line));
                    i += 2
                } else if i + 1 < cs.len() && cs[i + 1] == '=' {
                    out.push((Tok::MinusEq, line));
                    i += 2
                } else {
                    out.push((Tok::Minus, line));
                    i += 1
                }
            }
            '*' => {
                if i + 1 < cs.len() && cs[i + 1] == '*' {
                    out.push((Tok::StarStar, line));
                    i += 2
                } else if i + 1 < cs.len() && cs[i + 1] == '=' {
                    out.push((Tok::StarEq, line));
                    i += 2
                } else {
                    out.push((Tok::Star, line));
                    i += 1
                }
            }
            '/' => two!('=', Tok::SlashEq, Tok::Slash),
            '%' => {
                out.push((Tok::Percent, line));
                i += 1
            }
            '=' => two!('=', Tok::EqEq, Tok::Eq),
            '!' => two!('=', Tok::Ne, Tok::Bang),
            '<' => two!('=', Tok::Le, Tok::Lt),
            '>' => two!('=', Tok::Ge, Tok::Gt),
            '|' => {
                out.push((Tok::Pipe, line));
                i += 1
            }
            _ => return Err(format!("line {line}: unexpected character {c:?}")),
        }
    }
    out.push((Tok::Eof, line));
    Ok(out)
}
