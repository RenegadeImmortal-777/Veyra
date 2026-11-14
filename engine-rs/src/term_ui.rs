// Aesthetic Super-Terminal Engine (`kovrek terminal`) unifying Kovrek REPL,
// Git Bash / POSIX ergonomics, PowerShell, CMD, persistent history, and Tab completion.

#![allow(clippy::too_many_arguments)]
#![cfg_attr(not(windows), allow(dead_code, unused_variables))]

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::VERSION;
use crate::cli::exec_file_path;
use crate::server::NotebookSession;
use crate::value::Val;

const HISTORY_LIMIT: usize = 500;

const KOVREK_COMPLETIONS: &[&str] = &[
    ".help",
    ".run",
    ".watch",
    ".check",
    ".verify",
    ".bundle",
    ".vars",
    ".clear",
    ".history",
    ".version",
    ".exit",
    "let",
    "const",
    "fn",
    "async",
    "class",
    "if",
    "else",
    "for",
    "while",
    "return",
    "try",
    "catch",
    "match",
    "import",
    "use",
    "print",
    "input",
    "gui_app",
    "gui_message",
    "gui_input",
    "shell",
    "powershell",
    "bash_exec",
    "len",
    "str",
    "int",
    "float",
    "sqrt",
    "sin",
    "cos",
    "tan",
    "pow",
    "round",
    "json_parse",
    "json_stringify",
    "read_file",
    "write_file",
    "cd",
    "ls",
    "dir",
    "pwd",
    "cat",
    "touch",
    "mkdir",
    "rm",
    "cp",
    "mv",
    "which",
    "where",
    "env",
    "export",
    "clear",
    "cls",
    "ps",
    "bash",
    "git",
    "cargo",
    "python",
    "kovrek",
    "history",
    "exit",
    "quit",
];

fn ansi(use_color: bool, code: &str) -> &str {
    if use_color { code } else { "" }
}

fn c_reset(c: bool) -> &'static str {
    ansi(c, "\x1b[0m")
}
fn c_bold(c: bool) -> &'static str {
    ansi(c, "\x1b[1m")
}
fn c_cyan(c: bool) -> &'static str {
    ansi(c, "\x1b[38;2;103;232;249m")
}
fn c_blue(c: bool) -> &'static str {
    ansi(c, "\x1b[38;2;122;162;247m")
}
fn c_violet(c: bool) -> &'static str {
    ansi(c, "\x1b[38;2;187;154;247m")
}
fn c_green(c: bool) -> &'static str {
    ansi(c, "\x1b[38;2;158;206;106m")
}
fn c_amber(c: bool) -> &'static str {
    ansi(c, "\x1b[38;2;224;175;104m")
}
fn c_red(c: bool) -> &'static str {
    ansi(c, "\x1b[38;2;247;118;142m")
}
fn c_muted(c: bool) -> &'static str {
    ansi(c, "\x1b[38;2;130;140;170m")
}

pub(crate) fn colors_enabled() -> bool {
    io::stdout().is_terminal() && env::var_os("NO_COLOR").is_none()
}

#[cfg(windows)]
mod win_console {
    use libloading::Library;
    use std::sync::OnceLock;

    const STD_INPUT_HANDLE: i32 = -10;
    const STD_OUTPUT_HANDLE: i32 = -11;
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;
    const ENABLE_LINE_INPUT: u32 = 0x0002;
    const ENABLE_ECHO_INPUT: u32 = 0x0004;
    const ENABLE_PROCESSED_INPUT: u32 = 0x0001;
    const KEY_EVENT: u16 = 0x0001;

    pub(super) const VK_BACK: u16 = 0x08;
    pub(super) const VK_TAB: u16 = 0x09;
    pub(super) const VK_RETURN: u16 = 0x0D;
    pub(super) const VK_END: u16 = 0x23;
    pub(super) const VK_HOME: u16 = 0x24;
    pub(super) const VK_LEFT: u16 = 0x25;
    pub(super) const VK_UP: u16 = 0x26;
    pub(super) const VK_RIGHT: u16 = 0x27;
    pub(super) const VK_DOWN: u16 = 0x28;
    pub(super) const VK_DELETE: u16 = 0x2E;

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub(super) struct KeyEventRecord {
        pub(super) b_key_down: i32,
        pub(super) w_repeat_count: u16,
        pub(super) w_virtual_key_code: u16,
        pub(super) w_virtual_scan_code: u16,
        pub(super) unicode_char: u16,
        pub(super) dw_control_key_state: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct InputRecord {
        event_type: u16,
        _pad: u16,
        key: KeyEventRecord,
    }

    struct ConApi {
        _k32: Library,
        get_std_handle: unsafe extern "system" fn(i32) -> isize,
        get_console_mode: unsafe extern "system" fn(isize, *mut u32) -> i32,
        set_console_mode: unsafe extern "system" fn(isize, u32) -> i32,
        read_console_input:
            unsafe extern "system" fn(isize, *mut InputRecord, u32, *mut u32) -> i32,
    }
    unsafe impl Send for ConApi {}
    unsafe impl Sync for ConApi {}

    static CON_API: OnceLock<Option<ConApi>> = OnceLock::new();

    fn api() -> Option<&'static ConApi> {
        CON_API
            .get_or_init(|| {
                let k32 = unsafe { Library::new("kernel32.dll") }.ok()?;
                Some(ConApi {
                    get_std_handle: *unsafe { k32.get(b"GetStdHandle\0") }.ok()?,
                    get_console_mode: *unsafe { k32.get(b"GetConsoleMode\0") }.ok()?,
                    set_console_mode: *unsafe { k32.get(b"SetConsoleMode\0") }.ok()?,
                    read_console_input: *unsafe { k32.get(b"ReadConsoleInputW\0") }.ok()?,
                    _k32: k32,
                })
            })
            .as_ref()
    }

    pub(super) fn enable_vt_output() {
        let Some(a) = api() else {
            return;
        };
        let hout = unsafe { (a.get_std_handle)(STD_OUTPUT_HANDLE) };
        if hout == 0 || hout == -1 {
            return;
        }
        let mut mode = 0u32;
        if unsafe { (a.get_console_mode)(hout, &mut mode) } != 0 {
            let _ =
                unsafe { (a.set_console_mode)(hout, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) };
        }
    }

    pub(super) struct RawInputGuard {
        handle: isize,
        orig_mode: u32,
    }

    impl RawInputGuard {
        pub(super) fn enter() -> Option<Self> {
            let a = api()?;
            let hin = unsafe { (a.get_std_handle)(STD_INPUT_HANDLE) };
            if hin == 0 || hin == -1 {
                return None;
            }
            let mut orig = 0u32;
            if unsafe { (a.get_console_mode)(hin, &mut orig) } == 0 {
                return None;
            }
            let raw = orig & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT);
            if unsafe { (a.set_console_mode)(hin, raw) } == 0 {
                return None;
            }
            Some(Self {
                handle: hin,
                orig_mode: orig,
            })
        }

        pub(super) fn next_key(&self) -> Option<KeyEventRecord> {
            let a = api()?;
            loop {
                let mut rec = InputRecord {
                    event_type: 0,
                    _pad: 0,
                    key: KeyEventRecord {
                        b_key_down: 0,
                        w_repeat_count: 0,
                        w_virtual_key_code: 0,
                        w_virtual_scan_code: 0,
                        unicode_char: 0,
                        dw_control_key_state: 0,
                    },
                };
                let mut read = 0u32;
                if unsafe { (a.read_console_input)(self.handle, &mut rec, 1, &mut read) } == 0
                    || read == 0
                {
                    return None;
                }
                if rec.event_type == KEY_EVENT && rec.key.b_key_down != 0 {
                    return Some(rec.key);
                }
            }
        }
    }

    impl Drop for RawInputGuard {
        fn drop(&mut self) {
            if let Some(a) = api() {
                let _ = unsafe { (a.set_console_mode)(self.handle, self.orig_mode) };
            }
        }
    }
}

pub(crate) fn enable_virtual_terminal() {
    #[cfg(windows)]
    win_console::enable_vt_output();
}

pub(crate) struct TerminalState {
    pub(crate) prev_dir: Option<PathBuf>,
    pub(crate) last_exit: i32,
    pub(crate) history: Vec<String>,
    pub(crate) env_overrides: HashMap<String, String>,
    pub(crate) history_file: Option<PathBuf>,
}

impl TerminalState {
    pub(crate) fn new() -> Self {
        let history_file = history_store_path();
        let history = history_file
            .as_ref()
            .and_then(|p| fs::read_to_string(p).ok())
            .map(|txt| {
                txt.lines()
                    .map(str::trim_end)
                    .filter(|l| !l.is_empty())
                    .map(ToString::to_string)
                    .collect()
            })
            .unwrap_or_default();
        Self {
            prev_dir: None,
            last_exit: 0,
            history,
            env_overrides: HashMap::new(),
            history_file,
        }
    }

    pub(crate) fn push_history(&mut self, entry: &str) {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            return;
        }
        if self.history.last().map(String::as_str) == Some(trimmed) {
            return;
        }
        self.history.push(trimmed.to_string());
        if self.history.len() > HISTORY_LIMIT {
            let drain = self.history.len() - HISTORY_LIMIT;
            self.history.drain(..drain);
        }
        if let Some(path) = &self.history_file {
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let _ = fs::write(path, self.history.join("\n"));
        }
    }
}

fn history_store_path() -> Option<PathBuf> {
    if let Ok(local) = env::var("LOCALAPPDATA")
        && !local.trim().is_empty()
    {
        return Some(PathBuf::from(local).join("Kovrek").join("history.txt"));
    }
    home_dir().map(|h| h.join(".kovrek_history"))
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("USERPROFILE")
        .or_else(|| env::var_os("HOME"))
        .map(PathBuf::from)
}

fn shorten_cwd(path: &Path) -> String {
    let raw = path.to_string_lossy().to_string();
    if let Some(home) = home_dir() {
        let home_str = home.to_string_lossy().to_string();
        if let Some(rest) = raw.strip_prefix(&home_str) {
            if rest.is_empty() {
                return "~".into();
            }
            return format!("~{}", rest.replace('\\', "/"));
        }
    }
    raw.replace('\\', "/")
}

fn detect_git_branch(cwd: &Path) -> Option<String> {
    let mut cur = Some(cwd);
    while let Some(dir) = cur {
        let head = dir.join(".git").join("HEAD");
        if head.is_file() {
            let txt = fs::read_to_string(&head).ok()?;
            let line = txt.trim();
            if let Some(branch) = line.strip_prefix("ref: refs/heads/") {
                return Some(branch.to_string());
            }
            return Some(line.chars().take(7).collect());
        }
        cur = dir.parent();
    }
    None
}

pub(crate) fn print_banner(state: &TerminalState) {
    let c = colors_enabled();
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let cwd_disp = shorten_cwd(&cwd);
    let branch = detect_git_branch(&cwd).unwrap_or_else(|| "none".into());
    let gpu = crate::gpu::gpu_info_val();
    let gpu_name = if let Val::Map(pairs) = gpu {
        crate::value::map_lookup(&pairs, &Val::Str("backend".into()))
            .map(Val::show)
            .unwrap_or_else(|| "cpu".into())
    } else {
        "cpu".into()
    };
    let hist_n = state.history.len();

    println!(
        "{}{}╭────────────────────────────────────────────────────────────────────╮{}",
        c_bold(c),
        c_blue(c),
        c_reset(c)
    );
    println!(
        "{}{}│{} {}{}◆ Kovrek {} Super-Terminal{}                             {}│{}",
        c_bold(c),
        c_blue(c),
        c_reset(c),
        c_bold(c),
        c_cyan(c),
        VERSION,
        c_reset(c),
        c_blue(c),
        c_reset(c)
    );
    println!(
        "{}{}│{} {}Kovrek REPL{} + {}Git Bash{} (ls,cat,bash) + {}PowerShell{} (ps) + {}CMD{} (!){} │{}",
        c_bold(c),
        c_blue(c),
        c_reset(c),
        c_violet(c),
        c_reset(c),
        c_green(c),
        c_reset(c),
        c_cyan(c),
        c_reset(c),
        c_amber(c),
        c_reset(c),
        c_blue(c),
        c_reset(c)
    );
    println!(
        "{}{}│{} {}cwd:{} {}   {}git:{} {}   {}compute:{} {}   {}history:{} {}      {}│{}",
        c_bold(c),
        c_blue(c),
        c_reset(c),
        c_muted(c),
        c_reset(c),
        cwd_disp,
        c_muted(c),
        c_reset(c),
        branch,
        c_muted(c),
        c_reset(c),
        gpu_name,
        c_muted(c),
        c_reset(c),
        hist_n,
        c_blue(c),
        c_reset(c)
    );
    println!(
        "{}{}╰────────────────────────────────────────────────────────────────────╯{}",
        c_bold(c),
        c_blue(c),
        c_reset(c)
    );
    println!(
        "{}Type Kovrek code, shell/git commands, or {}.help{} (Up/Down history, Tab complete, .exit){}",
        c_muted(c),
        c_cyan(c),
        c_muted(c),
        c_reset(c)
    );
}

pub(crate) fn render_prompt(
    state: &TerminalState,
    session: &NotebookSession,
    continuation: bool,
) -> (String, usize) {
    let c = colors_enabled();
    if continuation {
        let vis_len = 5;
        let prompt = format!("{}...{}  ", c_muted(c), c_reset(c));
        return (prompt, vis_len);
    }
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let cwd_disp = shorten_cwd(&cwd);
    let vars_n = session.session_vars().len();
    let mut header = format!(
        "\n{}{}╭─ {}{}{}",
        c_blue(c),
        c_bold(c),
        c_cyan(c),
        cwd_disp,
        c_reset(c)
    );
    if let Some(br) = detect_git_branch(&cwd) {
        header.push_str(&format!("  {}git:({}){}", c_green(c), br, c_reset(c)));
    }
    if vars_n > 0 {
        header.push_str(&format!("  {}[{} vars]{}", c_violet(c), vars_n, c_reset(c)));
    }
    if state.last_exit != 0 {
        header.push_str(&format!(
            "  {}[exit {}]{}",
            c_red(c),
            state.last_exit,
            c_reset(c)
        ));
    }
    println!("{header}");
    let _ = io::stdout().flush();

    let prompt_ansi = if c {
        format!("{}╰─ kvr {}❯{} ", c_blue(c), c_violet(c), c_reset(c))
    } else {
        "kvr> ".into()
    };
    let vis_len = if c { 9 } else { 5 };
    (prompt_ansi, vis_len)
}

fn common_prefix(items: &[String]) -> String {
    let Some(first) = items.first() else {
        return String::new();
    };
    let mut prefix: Vec<char> = first.chars().collect();
    for item in items.iter().skip(1) {
        let chars: Vec<char> = item.chars().collect();
        let mut n = 0usize;
        while n < prefix.len() && n < chars.len() && prefix[n] == chars[n] {
            n += 1;
        }
        prefix.truncate(n);
    }
    prefix.into_iter().collect()
}

fn gather_completions(token: &str, session: &NotebookSession) -> Vec<String> {
    let mut out = Vec::new();
    for &kw in KOVREK_COMPLETIONS {
        if kw.starts_with(token) {
            out.push(kw.to_string());
        }
    }
    for (name, _) in session.session_vars() {
        if name.starts_with(token) && !out.contains(&name) {
            out.push(name);
        }
    }
    let clean_tok = token.trim_start_matches('"');
    let (dir_path, file_prefix) = match clean_tok.rfind(['/', '\\']) {
        Some(idx) => {
            let parent = &clean_tok[..=idx];
            (PathBuf::from(parent), &clean_tok[idx + 1..])
        }
        None => (PathBuf::from("."), clean_tok),
    };
    if let Ok(entries) = fs::read_dir(&dir_path) {
        for entry in entries.flatten() {
            let fname = entry.file_name().to_string_lossy().to_string();
            if fname.starts_with(file_prefix) {
                let mut candidate = if dir_path == Path::new(".") {
                    fname
                } else {
                    format!("{}{}", dir_path.to_string_lossy(), fname)
                };
                if entry.path().is_dir() {
                    candidate.push('/');
                }
                if candidate.contains(' ') {
                    candidate = format!("\"{candidate}\"");
                }
                if !out.contains(&candidate) {
                    out.push(candidate);
                }
            }
        }
    }
    out.sort();
    out
}

pub(crate) fn read_interactive_line(
    prompt: &str,
    _prompt_vis_len: usize,
    state: &TerminalState,
    session: &NotebookSession,
) -> Result<Option<String>, String> {
    #[cfg(windows)]
    if io::stdin().is_terminal() && io::stdout().is_terminal() {
        if let Some(guard) = win_console::RawInputGuard::enter() {
            return read_win_console_line(prompt, &guard, state, session);
        }
    }

    print!("{prompt}");
    io::stdout().flush().map_err(|e| e.to_string())?;
    let mut line = String::new();
    let n = io::stdin()
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    if n == 0 {
        return Ok(None);
    }
    while line.ends_with('\n') || line.ends_with('\r') {
        line.pop();
    }
    Ok(Some(line))
}

#[cfg(windows)]
fn redraw_line(prompt: &str, chars: &[char], cursor: usize) {
    let text: String = chars.iter().collect();
    let tail_chars = chars.len().saturating_sub(cursor);
    if tail_chars > 0 {
        print!("\r{prompt}{text}\x1b[K\x1b[{tail_chars}D");
    } else {
        print!("\r{prompt}{text}\x1b[K");
    }
    let _ = io::stdout().flush();
}

#[cfg(windows)]
fn read_win_console_line(
    prompt: &str,
    guard: &win_console::RawInputGuard,
    state: &TerminalState,
    session: &NotebookSession,
) -> Result<Option<String>, String> {
    use win_console::*;
    print!("{prompt}");
    io::stdout().flush().map_err(|e| e.to_string())?;

    let mut chars: Vec<char> = Vec::new();
    let mut cursor = 0usize;
    let mut hist_idx: Option<usize> = None;
    let mut saved_draft: Vec<char> = Vec::new();

    loop {
        let Some(k) = guard.next_key() else {
            return Ok(None);
        };
        let ctrl = (k.dw_control_key_state & 0x000C) != 0;
        let uch = k.unicode_char;

        if ctrl && (uch == 3 || uch == b'c' as u16 || k.w_virtual_key_code == b'C' as u16) {
            println!("^C");
            return Ok(Some(String::new()));
        }
        if ctrl && (uch == 4 || k.w_virtual_key_code == b'D' as u16) && chars.is_empty() {
            println!();
            return Ok(None);
        }
        if ctrl && (uch == 12 || k.w_virtual_key_code == b'L' as u16) {
            print!("\x1b[2J\x1b[H");
            redraw_line(prompt, &chars, cursor);
            continue;
        }

        match k.w_virtual_key_code {
            VK_RETURN => {
                println!();
                return Ok(Some(chars.into_iter().collect()));
            }
            VK_BACK => {
                if cursor > 0 {
                    cursor -= 1;
                    chars.remove(cursor);
                    redraw_line(prompt, &chars, cursor);
                }
            }
            VK_DELETE => {
                if cursor < chars.len() {
                    chars.remove(cursor);
                    redraw_line(prompt, &chars, cursor);
                }
            }
            VK_LEFT => {
                if cursor > 0 {
                    cursor -= 1;
                    redraw_line(prompt, &chars, cursor);
                }
            }
            VK_RIGHT => {
                if cursor < chars.len() {
                    cursor += 1;
                    redraw_line(prompt, &chars, cursor);
                }
            }
            VK_HOME => {
                cursor = 0;
                redraw_line(prompt, &chars, cursor);
            }
            VK_END => {
                cursor = chars.len();
                redraw_line(prompt, &chars, cursor);
            }
            VK_UP => {
                if !state.history.is_empty() {
                    let next = match hist_idx {
                        None => {
                            saved_draft = chars.clone();
                            state.history.len() - 1
                        }
                        Some(0) => 0,
                        Some(i) => i - 1,
                    };
                    hist_idx = Some(next);
                    chars = state.history[next].chars().collect();
                    cursor = chars.len();
                    redraw_line(prompt, &chars, cursor);
                }
            }
            VK_DOWN => {
                if let Some(i) = hist_idx {
                    if i + 1 < state.history.len() {
                        hist_idx = Some(i + 1);
                        chars = state.history[i + 1].chars().collect();
                    } else {
                        hist_idx = None;
                        chars = saved_draft.clone();
                    }
                    cursor = chars.len();
                    redraw_line(prompt, &chars, cursor);
                }
            }
            VK_TAB => {
                let left_text: String = chars[..cursor].iter().collect();
                let start_byte = left_text
                    .rfind(|c: char| c.is_whitespace())
                    .map(|idx| idx + 1)
                    .unwrap_or(0);
                let char_start = left_text[..start_byte].chars().count();
                let token = &left_text[start_byte..];
                let matches = gather_completions(token, session);
                if matches.len() == 1 {
                    let replacement = &matches[0];
                    let after: Vec<char> = chars[cursor..].to_vec();
                    chars.truncate(char_start);
                    chars.extend(replacement.chars());
                    cursor = chars.len();
                    chars.extend(after);
                    redraw_line(prompt, &chars, cursor);
                } else if matches.len() > 1 {
                    let cp = common_prefix(&matches);
                    if cp.chars().count() > token.chars().count() {
                        let after: Vec<char> = chars[cursor..].to_vec();
                        chars.truncate(char_start);
                        chars.extend(cp.chars());
                        cursor = chars.len();
                        chars.extend(after);
                    } else {
                        println!();
                        let preview: Vec<&str> =
                            matches.iter().take(12).map(String::as_str).collect();
                        let c = colors_enabled();
                        println!("{}{}{}", c_muted(c), preview.join("  "), c_reset(c));
                    }
                    redraw_line(prompt, &chars, cursor);
                }
            }
            _ => {
                if uch >= 0x20
                    && let Some(ch) = char::from_u32(uch as u32)
                {
                    let rep = k.w_repeat_count.max(1) as usize;
                    for _ in 0..rep {
                        chars.insert(cursor, ch);
                        cursor += 1;
                    }
                    redraw_line(prompt, &chars, cursor);
                }
            }
        }
    }
}

pub(crate) fn find_git_bash() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        for candidate in [
            r"C:\Program Files\Git\bin\bash.exe",
            r"C:\Program Files\Git\usr\bin\bash.exe",
            r"C:\Program Files (x86)\Git\bin\bash.exe",
        ] {
            let p = Path::new(candidate);
            if p.is_file() {
                return Some(p.to_path_buf());
            }
        }
        find_on_path("bash")
    }
    #[cfg(not(windows))]
    {
        for p in ["/bin/bash", "/usr/bin/bash"] {
            if Path::new(p).is_file() {
                return Some(PathBuf::from(p));
            }
        }
        find_on_path("bash")
    }
}

pub(crate) fn find_on_path(cmd: &str) -> Option<PathBuf> {
    let clean = cmd.trim().trim_matches('"');
    if clean.is_empty() {
        return None;
    }
    let p = Path::new(clean);
    if p.components().count() > 1 {
        if p.is_file() {
            return Some(p.to_path_buf());
        }
        #[cfg(windows)]
        for ext in ["exe", "cmd", "bat"] {
            let cand = p.with_extension(ext);
            if cand.is_file() {
                return Some(cand);
            }
        }
        return None;
    }
    let path_var = env::var_os("PATH")?;
    for dir in env::split_paths(&path_var) {
        let base = dir.join(clean);
        if base.is_file() {
            return Some(base);
        }
        #[cfg(windows)]
        {
            for ext in ["exe", "cmd", "bat", "com"] {
                let with_ext = dir.join(format!("{clean}.{ext}"));
                if with_ext.is_file() {
                    return Some(with_ext);
                }
            }
        }
    }
    None
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShellKind {
    System,
    PowerShell,
    GitBash,
}

fn run_shell_capture(
    kind: ShellKind,
    cmd: &str,
    env_map: &HashMap<String, String>,
) -> Result<(i32, String, String), String> {
    let mut command = match kind {
        ShellKind::System => {
            if cfg!(windows) {
                let mut c = Command::new("cmd");
                c.args(["/C", cmd]);
                c
            } else {
                let mut c = Command::new("sh");
                c.args(["-c", cmd]);
                c
            }
        }
        ShellKind::PowerShell => {
            let mut c = Command::new("powershell");
            c.args(["-NoProfile", "-Command", cmd]);
            c
        }
        ShellKind::GitBash => {
            let bash = find_git_bash().ok_or("Git Bash (bash.exe) not found on this machine")?;
            let mut c = Command::new(bash);
            c.args(["-c", cmd]);
            c
        }
    };
    command.envs(env_map);
    let out = command.output().map_err(|e| e.to_string())?;
    Ok((
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    ))
}

pub(crate) fn builtin_shell_val(kind: ShellKind, cmd: &str) -> Result<Val, String> {
    let empty = HashMap::new();
    let (code, stdout, stderr) = run_shell_capture(kind, cmd, &empty)?;
    Ok(Val::Map(vec![
        (Val::Str("code".into()), Val::Num(code as f64)),
        (Val::Str("out".into()), Val::Str(stdout)),
        (Val::Str("err".into()), Val::Str(stderr)),
    ]))
}

fn run_shell_interactive(
    kind: ShellKind,
    cmd: &str,
    state: &mut TerminalState,
) -> Result<(), String> {
    let mut command = match kind {
        ShellKind::System => {
            if cfg!(windows) {
                let mut c = Command::new("cmd");
                c.args(["/C", cmd]);
                c
            } else {
                let mut c = Command::new("sh");
                c.args(["-c", cmd]);
                c
            }
        }
        ShellKind::PowerShell => {
            let mut c = Command::new("powershell");
            c.args(["-NoProfile", "-Command", cmd]);
            c
        }
        ShellKind::GitBash => {
            let bash = find_git_bash().ok_or("Git Bash (bash.exe) not found on this machine")?;
            let mut c = Command::new(bash);
            c.args(["-c", cmd]);
            c
        }
    };
    command.envs(&state.env_overrides);
    let status = command.status().map_err(|e| e.to_string())?;
    state.last_exit = status.code().unwrap_or(1);
    Ok(())
}

fn expand_path(raw: &str) -> PathBuf {
    let trimmed = raw.trim().trim_matches('"');
    if trimmed == "~"
        && let Some(home) = home_dir()
    {
        return home;
    }
    if let Some(rest) = trimmed
        .strip_prefix("~/")
        .or_else(|| trimmed.strip_prefix("~\\"))
        && let Some(home) = home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(trimmed)
}

fn human_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", (bytes as f64) / 1024.0)
    } else {
        format!("{:.1} MB", (bytes as f64) / (1024.0 * 1024.0))
    }
}

fn cmd_ls(args: &[String]) -> Result<(), String> {
    let mut show_all = false;
    let mut target: Option<PathBuf> = None;
    for arg in args {
        if arg.starts_with('-') {
            if arg.contains('a') {
                show_all = true;
            }
        } else {
            target = Some(expand_path(arg));
        }
    }
    let dir = target.unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for entry in fs::read_dir(&dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name().to_string_lossy().to_string();
        if !show_all && name.starts_with('.') {
            continue;
        }
        let meta = entry.metadata();
        let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        if is_dir {
            dirs.push(name);
        } else {
            files.push((name, size));
        }
    }
    dirs.sort_by_key(|a| a.to_lowercase());
    files.sort_by_key(|(a, _)| a.to_lowercase());

    let c = colors_enabled();
    for d in &dirs {
        println!(
            "  {}[DIR]{}   {}{}{}/",
            c_cyan(c),
            c_reset(c),
            c_bold(c),
            d,
            c_reset(c)
        );
    }
    for (f, sz) in &files {
        let color = if f.ends_with(".kvr") || f.ends_with(".kvbc") {
            c_green(c)
        } else if f.ends_with(".exe") || f.ends_with(".bat") || f.ends_with(".ps1") {
            c_amber(c)
        } else {
            c_reset(c)
        };
        println!("  {:>8}  {}{}{}", human_size(*sz), color, f, c_reset(c));
    }
    println!(
        "{}{} dir(s), {} file(s){}",
        c_muted(c),
        dirs.len(),
        files.len(),
        c_reset(c)
    );
    Ok(())
}

fn cmd_cat(args: &[String]) -> Result<(), String> {
    if args.is_empty() {
        return Err("usage: cat <file>".into());
    }
    let c = colors_enabled();
    for arg in args {
        let p = expand_path(arg);
        let txt = fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        for (idx, line) in txt.lines().enumerate() {
            println!("{}{:>4} │{} {}", c_muted(c), idx + 1, c_reset(c), line);
        }
    }
    Ok(())
}

fn copy_path_recursive(src: &Path, dst: &Path) -> Result<(), String> {
    if src.is_dir() {
        fs::create_dir_all(dst).map_err(|e| e.to_string())?;
        for entry in fs::read_dir(src).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            copy_path_recursive(&entry.path(), &dst.join(entry.file_name()))?;
        }
    } else {
        fs::copy(src, dst).map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub(crate) fn watch_file(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Err(format!("file not found: {}", path.display()));
    }
    let c = colors_enabled();
    println!(
        "{}Watching {} — press Ctrl+C to stop{}",
        c_cyan(c),
        path.display(),
        c_reset(c)
    );
    let mut last_mod = fs::metadata(path).and_then(|m| m.modified()).ok();
    let _ = exec_file_path(path).map_err(|e| eprintln!("{}error: {e}{}", c_red(c), c_reset(c)));
    let start = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(250));
        if let Ok(meta) = fs::metadata(path)
            && let Ok(mod_t) = meta.modified()
            && Some(mod_t) != last_mod
        {
            last_mod = Some(mod_t);
            println!(
                "\n{}── reloaded ({:.1}s) ──{}",
                c_muted(c),
                start.elapsed().as_secs_f64(),
                c_reset(c)
            );
            if let Err(e) = exec_file_path(path) {
                eprintln!("{}error: {e}{}", c_red(c), c_reset(c));
            }
        }
    }
}

/// Parses capture assignment syntax such as:
///   `let out = !git status` | `let out = ps Get-Date` | `out = bash ls -1`
fn try_parse_shell_capture(
    line: &str,
    session: &NotebookSession,
) -> Option<(String, ShellKind, String)> {
    let trimmed = line.trim();
    let rest = trimmed.strip_prefix("let ").unwrap_or(trimmed).trim();
    let (lhs, rhs) = rest.split_once('=')?;
    if lhs.trim().is_empty() || rhs.starts_with('=') {
        return None;
    }
    let var = lhs.trim();
    if !var.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    let rhs_trim = rhs.trim();
    if let Some(cmd) = rhs_trim.strip_prefix('!') {
        let c = cmd.trim();
        let word = c.split_whitespace().next().unwrap_or("");
        if c.is_empty()
            || c.starts_with('(')
            || matches!(word, "true" | "false" | "null")
            || session.has_symbol(word)
        {
            return None;
        }
        return Some((var.to_string(), ShellKind::System, c.to_string()));
    }
    if let Some(cmd) = rhs_trim.strip_prefix("ps ") {
        return Some((
            var.to_string(),
            ShellKind::PowerShell,
            cmd.trim().to_string(),
        ));
    }
    if let Some(cmd) = rhs_trim.strip_prefix("bash ") {
        return Some((var.to_string(), ShellKind::GitBash, cmd.trim().to_string()));
    }
    None
}

fn looks_like_kovrek_code(line: &str, session: &NotebookSession) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return false;
    }
    let first_ch = t.chars().next().unwrap_or(' ');
    if first_ch.is_ascii_digit() || matches!(first_ch, '"' | '\'' | '[' | '{' | '(' | '-') {
        return true;
    }
    let head = t
        .split(|c: char| c.is_whitespace() || matches!(c, '(' | '=' | '+' | '-' | '*' | '/'))
        .next()
        .unwrap_or("");
    if matches!(
        head,
        "let"
            | "const"
            | "fn"
            | "async"
            | "class"
            | "if"
            | "while"
            | "for"
            | "try"
            | "match"
            | "return"
            | "throw"
            | "import"
            | "use"
            | "with"
            | "print"
            | "input"
            | "gui_app"
            | "true"
            | "false"
            | "null"
    ) {
        return true;
    }
    if session.has_symbol(head) {
        return true;
    }
    let after_head = t[head.len()..].trim_start();
    after_head.starts_with('(')
        || after_head.starts_with('=')
        || after_head.starts_with('+')
        || after_head.starts_with('-')
        || after_head.starts_with('*')
        || after_head.starts_with('/')
        || after_head.starts_with('[')
        || after_head.starts_with('.')
        || t.ends_with(';')
}

pub(crate) enum DispatchOutcome {
    Continue,
    Exit,
    PassToKovrek,
}

pub(crate) fn try_dispatch_shell(
    trimmed: &str,
    split_args: &[String],
    state: &mut TerminalState,
    session: &mut NotebookSession,
) -> Result<DispatchOutcome, String> {
    if trimmed == "exit" || trimmed == "quit" {
        return Ok(DispatchOutcome::Exit);
    }

    if let Some((var, kind, cmd)) = try_parse_shell_capture(trimmed, session) {
        let (code, stdout, stderr) = run_shell_capture(kind, &cmd, &state.env_overrides)?;
        state.last_exit = code;
        if !stderr.trim().is_empty() {
            eprint!("{stderr}");
        }
        let val = Val::Str(stdout.trim_end_matches(['\r', '\n']).to_string());
        let shown = val.show();
        session.set_var(var.clone(), val);
        let c = colors_enabled();
        println!("{}{var}{} = {}", c_violet(c), c_reset(c), shown);
        return Ok(DispatchOutcome::Continue);
    }

    if let Some(cmd) = trimmed.strip_prefix('!') {
        run_shell_interactive(ShellKind::System, cmd.trim(), state)?;
        return Ok(DispatchOutcome::Continue);
    }
    if trimmed == "ps" {
        run_shell_interactive(ShellKind::PowerShell, "", state)?;
        return Ok(DispatchOutcome::Continue);
    }
    if let Some(cmd) = trimmed.strip_prefix("ps ") {
        run_shell_interactive(ShellKind::PowerShell, cmd.trim(), state)?;
        return Ok(DispatchOutcome::Continue);
    }
    if trimmed == "bash" {
        run_shell_interactive(ShellKind::GitBash, "", state)?;
        return Ok(DispatchOutcome::Continue);
    }
    if let Some(cmd) = trimmed.strip_prefix("bash ") {
        run_shell_interactive(ShellKind::GitBash, cmd.trim(), state)?;
        return Ok(DispatchOutcome::Continue);
    }

    if let Some(rest) = trimmed.strip_prefix("$env:")
        && let Some((k, v)) = rest.split_once('=')
    {
        state.env_overrides.insert(
            k.trim().to_string(),
            v.trim().trim_matches('"').trim_matches('\'').to_string(),
        );
        state.last_exit = 0;
        return Ok(DispatchOutcome::Continue);
    }

    let Some(cmd) = split_args.first().map(String::as_str) else {
        return Ok(DispatchOutcome::Continue);
    };
    if (trimmed.contains('|') || trimmed.contains('>'))
        && matches!(cmd, "ls" | "ll" | "dir" | "cat" | "env")
    {
        let kind = if matches!(cmd, "ls" | "ll" | "cat") && find_git_bash().is_some() {
            ShellKind::GitBash
        } else {
            ShellKind::System
        };
        run_shell_interactive(kind, trimmed, state)?;
        return Ok(DispatchOutcome::Continue);
    }
    let rest_args = &split_args[1..];

    match cmd {
        "pwd" => {
            println!(
                "{}",
                env::current_dir().map_err(|e| e.to_string())?.display()
            );
            state.last_exit = 0;
            return Ok(DispatchOutcome::Continue);
        }
        "cd" => {
            let cur = env::current_dir().map_err(|e| e.to_string())?;
            let target = match rest_args.first().map(String::as_str) {
                None => home_dir().unwrap_or(cur.clone()),
                Some("-") => state.prev_dir.clone().unwrap_or(cur.clone()),
                Some(p) => expand_path(p),
            };
            env::set_current_dir(&target).map_err(|e| e.to_string())?;
            state.prev_dir = Some(cur);
            state.last_exit = 0;
            return Ok(DispatchOutcome::Continue);
        }
        "ls" | "ll" | "dir" => {
            cmd_ls(rest_args)?;
            state.last_exit = 0;
            return Ok(DispatchOutcome::Continue);
        }
        "clear" | "cls" => {
            print!("\x1b[2J\x1b[H");
            let _ = io::stdout().flush();
            state.last_exit = 0;
            return Ok(DispatchOutcome::Continue);
        }
        "cat" => {
            cmd_cat(rest_args)?;
            state.last_exit = 0;
            return Ok(DispatchOutcome::Continue);
        }
        "touch" => {
            let p = rest_args.first().ok_or("usage: touch <file>")?;
            let path = expand_path(p);
            if !path.exists() {
                fs::File::create(&path).map_err(|e| e.to_string())?;
            }
            state.last_exit = 0;
            return Ok(DispatchOutcome::Continue);
        }
        "mkdir" => {
            let p = rest_args
                .iter()
                .find(|a| !a.starts_with('-'))
                .ok_or("usage: mkdir <dir>")?;
            fs::create_dir_all(expand_path(p)).map_err(|e| e.to_string())?;
            state.last_exit = 0;
            return Ok(DispatchOutcome::Continue);
        }
        "rm" => {
            let rec = rest_args
                .iter()
                .any(|a| a == "-r" || a == "-rf" || a == "-fr");
            let target = rest_args
                .iter()
                .find(|a| !a.starts_with('-'))
                .ok_or("usage: rm [-r] <path>")?;
            let p = expand_path(target);
            if p.is_dir() {
                if rec {
                    fs::remove_dir_all(&p).map_err(|e| e.to_string())?;
                } else {
                    fs::remove_dir(&p).map_err(|e| e.to_string())?;
                }
            } else {
                fs::remove_file(&p).map_err(|e| e.to_string())?;
            }
            state.last_exit = 0;
            return Ok(DispatchOutcome::Continue);
        }
        "cp" => {
            if rest_args.len() != 2 {
                return Err("usage: cp <src> <dst>".into());
            }
            copy_path_recursive(&expand_path(&rest_args[0]), &expand_path(&rest_args[1]))?;
            state.last_exit = 0;
            return Ok(DispatchOutcome::Continue);
        }
        "mv" => {
            if rest_args.len() != 2 {
                return Err("usage: mv <src> <dst>".into());
            }
            fs::rename(expand_path(&rest_args[0]), expand_path(&rest_args[1]))
                .map_err(|e| e.to_string())?;
            state.last_exit = 0;
            return Ok(DispatchOutcome::Continue);
        }
        "which" | "where" => {
            let name = rest_args.first().ok_or("usage: which <command>")?;
            if let Some(found) = find_on_path(name) {
                println!("{}", found.display());
                state.last_exit = 0;
            } else {
                state.last_exit = 1;
                return Err(format!("not found on PATH: {name}"));
            }
            return Ok(DispatchOutcome::Continue);
        }
        "env" => {
            let mut pairs: Vec<(String, String)> = env::vars().collect();
            for (k, v) in &state.env_overrides {
                pairs.retain(|(ek, _)| ek != k);
                pairs.push((k.clone(), v.clone()));
            }
            pairs.sort_by(|a, b| a.0.cmp(&b.0));
            for (k, v) in pairs {
                println!("{k}={v}");
            }
            state.last_exit = 0;
            return Ok(DispatchOutcome::Continue);
        }
        "export" | "set" => {
            if rest_args.is_empty() {
                return try_dispatch_shell("env", &["env".into()], state, session);
            }
            for arg in rest_args {
                if let Some((k, v)) = arg.split_once('=') {
                    state
                        .env_overrides
                        .insert(k.trim().to_string(), v.trim().to_string());
                }
            }
            state.last_exit = 0;
            return Ok(DispatchOutcome::Continue);
        }
        "history" => {
            for (i, h) in state.history.iter().enumerate() {
                println!("  {:>3}  {}", i + 1, h);
            }
            state.last_exit = 0;
            return Ok(DispatchOutcome::Continue);
        }
        _ => {}
    }

    if !looks_like_kovrek_code(trimmed, session) && find_on_path(cmd).is_some() {
        run_shell_interactive(ShellKind::System, trimmed, state)?;
        return Ok(DispatchOutcome::Continue);
    }

    Ok(DispatchOutcome::PassToKovrek)
}
