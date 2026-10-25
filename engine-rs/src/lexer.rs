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
    Slash,
    Percent,
    Eq,
    EqEq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Bang,
    Arrow,
    Eof,
}

pub(crate) fn lex(src: &str) -> Result<Vec<Tok>, String> {
    let mut out = Vec::new();
    let cs: Vec<char> = src.chars().collect();
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '/' && i + 1 < cs.len() && cs[i + 1] == '/' {
            i += 2;
            while i < cs.len() && cs[i] != '\n' {
                i += 1;
            }
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
                .map_err(|_| "bad number")?;
            out.push(Tok::Num(n));
            continue;
        }
        if c == '"' {
            i += 1;
            let mut s = String::new();
            while i < cs.len() && cs[i] != '"' {
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
                return Err("unterminated string".into());
            }
            i += 1;
            out.push(Tok::Str(s));
            continue;
        }
        if c.is_ascii_alphabetic() || c == '_' {
            let s = i;
            i += 1;
            while i < cs.len() && (cs[i].is_ascii_alphanumeric() || cs[i] == '_') {
                i += 1;
            }
            let w: String = cs[s..i].iter().collect();
            out.push(match w.as_str() {
                "let" => Tok::Let,
                "const" => Tok::Const,
                "fn" => Tok::Fn,
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
                "catch" => Tok::Catch,
                "finally" => Tok::Finally,
                "throw" => Tok::Throw,
                "match" => Tok::Match,
                "case" => Tok::Case,
                "await" => Tok::Await,
                "with" => Tok::With,
                "as" => Tok::As,
                "and" => Tok::And,
                "or" => Tok::Or,
                "not" => Tok::Not,
                "true" => Tok::True,
                "false" => Tok::False,
                "null" => Tok::Null,
                _ => Tok::Id(w),
            });
            continue;
        }
        macro_rules! two {
            ($ch:expr,$eq:expr,$one:expr) => {{
                if i + 1 < cs.len() && cs[i + 1] == '=' {
                    i += 2;
                    out.push($eq);
                } else {
                    i += 1;
                    out.push($one);
                }
                continue;
            }};
        }
        match c {
            '(' => {
                out.push(Tok::Lp);
                i += 1
            }
            ')' => {
                out.push(Tok::Rp);
                i += 1
            }
            '{' => {
                out.push(Tok::Lb);
                i += 1
            }
            '}' => {
                out.push(Tok::Rb);
                i += 1
            }
            '[' => {
                out.push(Tok::Ls);
                i += 1
            }
            ']' => {
                out.push(Tok::Rs);
                i += 1
            }
            ',' => {
                out.push(Tok::Comma);
                i += 1
            }
            '@' => {
                out.push(Tok::At);
                i += 1
            }
            ':' => {
                out.push(Tok::Colon);
                i += 1
            }
            '.' => {
                out.push(Tok::Dot);
                i += 1
            }
            ';' => {
                out.push(Tok::Semi);
                i += 1
            }
            '+' => {
                out.push(Tok::Plus);
                i += 1
            }
            '-' => {
                if i + 1 < cs.len() && cs[i + 1] == '>' {
                    out.push(Tok::Arrow);
                    i += 2
                } else {
                    out.push(Tok::Minus);
                    i += 1
                }
            }
            '*' => {
                out.push(Tok::Star);
                i += 1
            }
            '/' => {
                out.push(Tok::Slash);
                i += 1
            }
            '%' => {
                out.push(Tok::Percent);
                i += 1
            }
            '=' => two!('=', Tok::EqEq, Tok::Eq),
            '!' => two!('=', Tok::Ne, Tok::Bang),
            '<' => two!('=', Tok::Le, Tok::Lt),
            '>' => two!('=', Tok::Ge, Tok::Gt),
            _ => return Err(format!("unexpected character {c:?}")),
        }
    }
    out.push(Tok::Eof);
    Ok(out)
}
