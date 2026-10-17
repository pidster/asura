//! Shell line splitter and tokenizer, ported from Wisp `CommandSplitter`
//! (`tokens(of:)`, `segments(of:)`, `substitutions(in:)`, `innerScripts(of:)`
//! and their helpers) at commit 4305d2c. A pure pre-pass for command display
//! and secret rewrite: not a shell, no I/O, and it grants no authority.
//!
//! The port works on bytes. Every byte Wisp treats specially is ASCII, so
//! UTF-8 text splits the same way, except that only ASCII whitespace
//! separates words (as in `/bin/sh`), where Wisp also used Unicode spaces.
use std::collections::HashSet;
use std::ops::Range;

/// One word of a segment: its shell value (quotes removed, escapes read) and
/// its raw byte range in the analysed text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Word {
    pub value: String,
    pub raw: Range<usize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Quoting {
    None,
    Single,
    Double,
    /// `$'…'`
    Ansi,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Role {
    /// Unquoted whitespace, operators and `$IFS`, or heredoc body bytes.
    Separator,
    /// A byte that is part of a word's value.
    Text,
    /// An opening or closing quote, including the `$` of `$'`.
    QuoteMark,
    /// A backslash that escapes the next character.
    Escape,
    /// A character escaped by a backslash.
    Escaped,
}
/// The quoting context and nesting of one source byte.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ByteContext {
    pub quoting: Quoting,
    pub role: Role,
    /// 0-based index into `Analysis::words`.
    pub word: Option<usize>,
    pub heredoc: bool,
    pub substitution: bool,
    /// The script word of `sh -c` and similar, or a word after `eval`.
    pub inner_script: bool,
}
impl Default for ByteContext {
    fn default() -> Self {
        Self {
            quoting: Quoting::None,
            role: Role::Separator,
            word: None,
            heredoc: false,
            substitution: false,
            inner_script: false,
        }
    }
}

/// Per-byte contexts of a whole command. `parsed` is false for an unclosed
/// quote, substitution or group, or a heredoc operator without a delimiter;
/// the splitter never guesses.
#[derive(Clone, Debug)]
pub struct Analysis {
    pub parsed: bool,
    pub bytes: Vec<ByteContext>,
    pub words: Vec<Range<usize>>,
}

const WRAPPERS: &[&str] = &[
    "sudo",
    "doas",
    "env",
    "nice",
    "nohup",
    "time",
    "timeout",
    "xargs",
    "command",
    "exec",
    "builtin",
    "caffeinate",
    "stdbuf",
    "ionice",
    "chronic",
];
const VALUE_OPTIONS: &[&str] = &[
    "-n", "-u", "-g", "-s", "-k", "-C", "-i", "-p", "-P", "-I", "-L", "-a", "-c",
];
const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh", "fish"];
const LEADING_KEYWORDS: &[&str] = &[
    "do", "then", "else", "elif", "if", "while", "until", "!", "{",
];
const CLOSING_KEYWORDS: &[&str] = &["done", "fi", "esac", "}"];

fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}
fn utf8_len(lead: u8) -> usize {
    match lead {
        0xf0..=0xff => 4,
        0xe0..=0xef => 3,
        0xc0..=0xdf => 2,
        _ => 1,
    }
}

/// Top-level segments split on `;`, newline, `|`, `||`, `&&` and `&` outside
/// quotes and parentheses. Ranges exclude the operators.
pub fn segments(line: &str) -> Vec<Range<usize>> {
    segment_ranges(line.as_bytes())
}
fn segment_ranges(b: &[u8]) -> Vec<Range<usize>> {
    let (mut single, mut double, mut ansi, mut backtick) = (false, false, false, false);
    let mut depth = 0usize;
    let mut result = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let next = b.get(i + 1).copied();
        if single {
            single = c != b'\'';
        } else if ansi {
            if c == b'\\' && next.is_some() {
                i += 1;
            } else if c == b'\'' {
                ansi = false;
            }
        } else if double {
            if c == b'\\' && next.is_some() {
                i += 1;
            } else if c == b'"' {
                double = false;
            }
        } else if backtick {
            backtick = c != b'`';
        } else {
            match c {
                b'\\' if next.is_some() => i += 1,
                b'$' if next == Some(b'\'') => {
                    ansi = true;
                    i += 1;
                }
                b'\'' => single = true,
                b'"' => double = true,
                b'`' => backtick = true,
                b'(' => depth += 1,
                b')' => depth = depth.saturating_sub(1),
                b';' | b'\n' if depth == 0 => {
                    result.push(start..i);
                    start = i + 1;
                }
                b'|' if depth == 0 => {
                    result.push(start..i);
                    if next == Some(b'|') {
                        i += 1;
                    }
                    start = i + 1;
                }
                b'&' if depth == 0 => {
                    if next == Some(b'&') {
                        result.push(start..i);
                        i += 1;
                        start = i + 1;
                    } else if next == Some(b'>') || (i > start && b[i - 1] == b'>') {
                        // Redirection such as `2>&1` or `&>`.
                    } else {
                        result.push(start..i);
                        start = i + 1;
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    result.push(start..b.len());
    result
}

/// The index just past the quoted text whose opening quote is at `open`.
fn quote_end(b: &[u8], open: usize) -> usize {
    let quote = b[open];
    let escapes = quote == b'"' || (open > 0 && b[open - 1] == b'$' && quote == b'\'');
    let mut cursor = open + 1;
    while cursor < b.len() {
        if escapes && b[cursor] == b'\\' {
            cursor += 2;
            continue;
        }
        if b[cursor] == quote {
            return cursor + 1;
        }
        cursor += 1;
    }
    b.len()
}
fn closing_parenthesis(b: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0i64;
    let mut cursor = open;
    while cursor < b.len() {
        match b[cursor] {
            b'\\' => {
                cursor += 2;
                continue;
            }
            b'\'' | b'"' => {
                cursor = quote_end(b, cursor);
                continue;
            }
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(cursor);
                }
            }
            _ => {}
        }
        cursor += 1;
    }
    None
}

/// One outermost `$(…)`, backtick or `(…)` group: its body and whole range.
struct Group {
    body: Range<usize>,
    whole: Range<usize>,
}
/// Wisp `substitutions(in:)`, also reporting an unclosed group.
fn groups(b: &[u8]) -> (Vec<Group>, bool) {
    let mut found = Vec::new();
    let mut closed = true;
    let mut double = false;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'"' {
            double = !double;
            i += 1;
            continue;
        }
        if !double && (c == b'\'' || (c == b'$' && b.get(i + 1) == Some(&b'\''))) {
            i = quote_end(b, if c == b'$' { i + 1 } else { i });
            continue;
        }
        if c == b'\\' {
            i += 2;
            continue;
        }
        if double && c == b'(' {
            i += 1;
            continue;
        }
        if c == b'`' {
            match b[i + 1..].iter().position(|&x| x == b'`') {
                Some(offset) => {
                    let close = i + 1 + offset;
                    found.push(Group {
                        body: i + 1..close,
                        whole: i..close + 1,
                    });
                    i = close + 1;
                    continue;
                }
                None => closed = false,
            }
        }
        let dollar = c == b'$' && b.get(i + 1) == Some(&b'(');
        if dollar || c == b'(' {
            let open = if dollar { i + 1 } else { i };
            match closing_parenthesis(b, open) {
                Some(close) => {
                    found.push(Group {
                        body: open + 1..close,
                        whole: i..close + 1,
                    });
                    i = close + 1;
                    continue;
                }
                None => closed = false,
            }
        }
        i += 1;
    }
    (found, closed)
}

/// The bodies of `$(…)`, backtick and top-level `(…)` groups, outermost only.
pub fn substitutions(segment: &str) -> Vec<Range<usize>> {
    groups(segment.as_bytes())
        .0
        .into_iter()
        .map(|g| g.body)
        .collect()
}

fn ifs_length(b: &[u8], i: usize) -> Option<usize> {
    let ahead = &b[i..b.len().min(i + 6)];
    if ahead == b"${IFS}" {
        return Some(6);
    }
    if !ahead.starts_with(b"$IFS") {
        return None;
    }
    match b.get(i + 4) {
        Some(&a) if a.is_ascii_alphanumeric() || a == b'_' || a >= 0x80 => None,
        _ => Some(4),
    }
}

struct Token {
    value: Vec<u8>,
    raw: Range<usize>,
}

/// Wisp `tokens(of:)` over `b`, recording each byte's context at `base`.
fn tokenize(b: &[u8], base: usize, mut ctx: Option<&mut [ByteContext]>) -> Vec<Token> {
    let mut result = Vec::new();
    let mut current: Vec<u8> = Vec::new();
    let mut start: Option<usize> = None;
    let (mut single, mut double, mut ansi) = (false, false, false);
    let set = |ctx: &mut Option<&mut [ByteContext]>, at: usize, quoting: Quoting, role: Role| {
        if let Some(ctx) = ctx.as_deref_mut() {
            ctx[base + at].quoting = quoting;
            ctx[base + at].role = role;
        }
    };
    let finish = |end: usize,
                  start: &mut Option<usize>,
                  current: &mut Vec<u8>,
                  result: &mut Vec<Token>,
                  ctx: &mut Option<&mut [ByteContext]>| {
        if let Some(begin) = start.take() {
            let raw = &b[begin..end];
            if !current.is_empty() || raw.contains(&b'\'') || raw.contains(&b'"') {
                result.push(Token {
                    value: std::mem::take(current),
                    raw: begin..end,
                });
            } else if let Some(ctx) = ctx.as_deref_mut() {
                for c in &mut ctx[base + begin..base + end] {
                    c.role = Role::Separator;
                }
            }
        }
        current.clear();
    };
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let quoted = single || double || ansi;
        if !quoted && is_space(c) {
            finish(i, &mut start, &mut current, &mut result, &mut ctx);
            set(&mut ctx, i, Quoting::None, Role::Separator);
            i += 1;
            continue;
        }
        if !quoted
            && c == b'$'
            && let Some(length) = ifs_length(b, i)
        {
            finish(i, &mut start, &mut current, &mut result, &mut ctx);
            for k in i..i + length {
                set(&mut ctx, k, Quoting::None, Role::Separator);
            }
            i += length;
            continue;
        }
        start.get_or_insert(i);
        let quoting = if single {
            Quoting::Single
        } else if ansi {
            Quoting::Ansi
        } else if double {
            Quoting::Double
        } else {
            Quoting::None
        };
        if single {
            if c == b'\'' {
                single = false;
                set(&mut ctx, i, Quoting::Single, Role::QuoteMark);
            } else {
                current.push(c);
                set(&mut ctx, i, Quoting::Single, Role::Text);
            }
        } else if c == b'\\' {
            set(&mut ctx, i, quoting, Role::Escape);
            if i + 1 < b.len() {
                i += 1;
                let length = utf8_len(b[i]).min(b.len() - i);
                if b[i] != b'\n' {
                    current.extend_from_slice(&b[i..i + length]);
                }
                for k in i..i + length {
                    set(&mut ctx, k, quoting, Role::Escaped);
                }
                i += length;
                continue;
            }
        } else if ansi {
            if c == b'\'' {
                ansi = false;
                set(&mut ctx, i, Quoting::Ansi, Role::QuoteMark);
            } else {
                current.push(c);
                set(&mut ctx, i, Quoting::Ansi, Role::Text);
            }
        } else if double {
            if c == b'"' {
                double = false;
                set(&mut ctx, i, Quoting::Double, Role::QuoteMark);
            } else {
                current.push(c);
                set(&mut ctx, i, Quoting::Double, Role::Text);
            }
        } else if c == b'\'' {
            if i > 0 && b[i - 1] == b'$' && current.last() == Some(&b'$') {
                current.pop();
                ansi = true;
                set(&mut ctx, i - 1, Quoting::Ansi, Role::QuoteMark);
                set(&mut ctx, i, Quoting::Ansi, Role::QuoteMark);
            } else {
                single = true;
                set(&mut ctx, i, Quoting::Single, Role::QuoteMark);
            }
        } else if c == b'"' {
            double = true;
            set(&mut ctx, i, Quoting::Double, Role::QuoteMark);
        } else {
            current.push(c);
            set(&mut ctx, i, Quoting::None, Role::Text);
        }
        i += 1;
    }
    finish(b.len(), &mut start, &mut current, &mut result, &mut ctx);
    result
}

/// Whitespace-separated words of a segment, honouring quotes and escapes, as
/// Wisp's `tokens(of:)` reads them. Ranges are relative to `segment`.
pub fn tokens(segment: &str) -> Vec<Word> {
    tokenize(segment.as_bytes(), 0, None)
        .into_iter()
        .map(|t| Word {
            value: String::from_utf8_lossy(&t.value).into_owned(),
            raw: t.raw,
        })
        .collect()
}

fn raw_text<'a>(b: &'a [u8], t: &Token) -> &'a [u8] {
    &b[t.raw.clone()]
}
fn basename(value: &[u8]) -> &[u8] {
    value
        .split(|&c| c == b'/')
        .rfind(|p| !p.is_empty())
        .unwrap_or(value)
}
fn eq(bytes: &[u8], text: &str) -> bool {
    bytes == text.as_bytes()
}
fn is_name_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80
}

/// Wisp `clause(of:caseOpen:)`: the index of the word where the command a
/// segment runs starts, or `None` for a loop header or a lone closing keyword.
fn clause(b: &[u8], words: &[Token], case_open: bool) -> (Option<usize>, i64) {
    let mut delta = 0;
    let mut in_case = case_open;
    let mut index = 0;
    while index < words.len() {
        let raw = raw_text(b, &words[index]);
        let next = words.get(index + 1).map(|w| raw_text(b, w));
        if LEADING_KEYWORDS.iter().any(|k| eq(raw, k)) {
            index += 1;
        } else if CLOSING_KEYWORDS.iter().any(|k| eq(raw, k)) && index == 0 {
            if eq(raw, "esac") {
                delta -= 1;
            }
            return ((words.len() > 1).then_some(0), delta);
        } else if eq(raw, "for") || eq(raw, "select") {
            return (None, delta);
        } else if eq(raw, "case") {
            if index + 2 < words.len() && eq(raw_text(b, &words[index + 2]), "in") {
                delta += 1;
                in_case = true;
                index += 3;
            } else {
                return (Some(index), delta);
            }
        } else if eq(raw, "function") && next.is_some() {
            index += 2;
            if index < words.len() && eq(raw_text(b, &words[index]), "()") {
                index += 1;
            }
        } else if next.is_some_and(|n| eq(n, "()")) {
            index += 2;
        } else if (raw.len() > 2 && raw.ends_with(b"()"))
            || (in_case && raw.ends_with(b")") && !raw.contains(&b'('))
        {
            // `f() {`, or a case branch's pattern.
            index += 1;
        } else {
            break;
        }
    }
    ((index < words.len()).then_some(index), delta)
}

/// Wisp `unwrap`: the index of the word in the program's place, after
/// assignments and wrappers with their options; `None` for assignments alone.
fn unwrap(b: &[u8], words: &[Token]) -> Option<(usize, HashSet<Vec<u8>>)> {
    let mut wrappers = HashSet::new();
    let mut i = 0;
    while i < words.len() {
        let (raw, value) = (raw_text(b, &words[i]), &words[i].value[..]);
        let assignment = !raw.starts_with(b"(")
            && !raw.starts_with(b"$(")
            && value.contains(&b'=')
            && !value.starts_with(b"=")
            && value
                .split(|&c| c == b'=')
                .next()
                .is_some_and(|name| name.iter().all(|&c| is_name_byte(c)));
        if assignment {
            i += 1;
            continue;
        }
        let name = basename(value);
        if WRAPPERS.iter().any(|w| eq(name, w)) {
            let timeout = eq(name, "timeout");
            wrappers.insert(name.to_vec());
            i += 1;
            while let Some(option) = words.get(i).map(|w| &w.value[..]) {
                let numeric =
                    timeout && std::str::from_utf8(option).is_ok_and(|s| s.parse::<f64>().is_ok());
                if !(option.starts_with(b"-") || numeric) {
                    break;
                }
                i += 1;
                if VALUE_OPTIONS.iter().any(|v| eq(option, v)) && i < words.len() {
                    i += 1;
                }
            }
            continue;
        }
        return Some((i, wrappers));
    }
    None
}

/// Wisp `innerScripts(of:)` as word indices: the script word of `sh -c` and
/// similar shells, every word after `eval`, and those inside `find -exec`.
fn inner_script_words(b: &[u8], words: &[Token]) -> Vec<usize> {
    let Some((program, _)) = unwrap(b, words) else {
        return Vec::new();
    };
    let raw = raw_text(b, &words[program]);
    if raw.starts_with(b"(") || raw.starts_with(b"$(") || raw.starts_with(b"`") {
        return Vec::new();
    }
    let name = basename(&words[program].value);
    let rest = program + 1;
    if eq(name, "eval") {
        return (rest..words.len()).collect();
    }
    if eq(name, "find") {
        let mut found = Vec::new();
        let mut i = rest;
        while i < words.len() {
            let v = &words[i].value[..];
            i += 1;
            if !["-exec", "-execdir", "-ok", "-okdir"]
                .iter()
                .any(|k| eq(v, k))
            {
                continue;
            }
            let begin = i;
            while i < words.len() && !eq(&words[i].value, ";") && !eq(&words[i].value, "+") {
                i += 1;
            }
            found.extend(
                inner_script_words(b, &words[begin..i])
                    .into_iter()
                    .map(|k| k + begin),
            );
        }
        return found;
    }
    if !SHELLS.iter().any(|s| eq(name, s)) {
        return Vec::new();
    }
    let mut i = rest;
    while i < words.len() {
        let w = &words[i].value[..];
        i += 1;
        if eq(w, "-o") || eq(w, "+o") {
            i += 1;
            continue;
        }
        if !(w.starts_with(b"-") || w.starts_with(b"+")) || eq(w, "--") || eq(w, "-") {
            return Vec::new();
        }
        if !w.starts_with(b"--") && w.contains(&b'c') {
            return if i < words.len() { vec![i] } else { Vec::new() };
        }
    }
    Vec::new()
}

fn shell_word(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_alphanumeric() || "_./{}=+:@,%-".contains(c));
    if plain {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

/// The command lines a segment hands to a shell: the argument of `sh -c`,
/// `bash -lc` and the like, the words after `eval` joined by spaces, and each
/// `find -exec` command quoted again as one line.
pub fn inner_scripts(segment: &str) -> Vec<String> {
    let b = segment.as_bytes();
    let words = tokenize(b, 0, None);
    let Some((program, _)) = unwrap(b, &words) else {
        return Vec::new();
    };
    let value = |t: &Token| String::from_utf8_lossy(&t.value).into_owned();
    let name = basename(&words[program].value);
    if eq(name, "eval") {
        let rest: Vec<_> = words[program + 1..].iter().map(value).collect();
        return if rest.is_empty() {
            Vec::new()
        } else {
            vec![rest.join(" ")]
        };
    }
    if eq(name, "find") {
        let mut commands = Vec::new();
        let mut i = program + 1;
        while i < words.len() {
            let v = value(&words[i]);
            i += 1;
            if !["-exec", "-execdir", "-ok", "-okdir"].contains(&v.as_str()) {
                continue;
            }
            let mut command = Vec::new();
            while i < words.len() {
                let next = value(&words[i]);
                i += 1;
                if next == ";" || next == "+" {
                    break;
                }
                command.push(shell_word(&next));
            }
            if !command.is_empty() {
                commands.push(command.join(" "));
            }
        }
        return commands;
    }
    inner_script_words(b, &words)
        .into_iter()
        .map(|i| value(&words[i]))
        .collect()
}

/// Heredoc bodies: from the line after an unquoted `<<` or `<<-` operator to
/// its delimiter line. `<<<` is a here-string, not a heredoc. Also reports
/// whether every quote closed and every operator had a delimiter.
fn heredocs(b: &[u8]) -> (Vec<Range<usize>>, bool) {
    #[derive(PartialEq)]
    enum Q {
        Unquoted,
        Single,
        Double,
        Ansi,
    }
    let mut state = Q::Unquoted;
    let mut parsed = true;
    let mut pending: Vec<(Vec<u8>, bool)> = Vec::new();
    let mut bodies = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match state {
            Q::Single => {
                if c == b'\'' {
                    state = Q::Unquoted;
                }
                i += 1;
            }
            Q::Ansi | Q::Double => {
                let close = if state == Q::Ansi { b'\'' } else { b'"' };
                if c == b'\\' {
                    i += 2;
                } else {
                    if c == close {
                        state = Q::Unquoted;
                    }
                    i += 1;
                }
            }
            Q::Unquoted => {
                if c == b'\\' {
                    i += 2;
                } else if c == b'\'' {
                    state = Q::Single;
                    i += 1;
                } else if c == b'"' {
                    state = Q::Double;
                    i += 1;
                } else if c == b'$' && b.get(i + 1) == Some(&b'\'') {
                    state = Q::Ansi;
                    i += 2;
                } else if c == b'<' && b.get(i + 1) == Some(&b'<') {
                    if b.get(i + 2) == Some(&b'<') {
                        i += 3;
                        continue;
                    }
                    let mut j = i + 2;
                    let strip = b.get(j) == Some(&b'-');
                    j += usize::from(strip);
                    while matches!(b.get(j), Some(b' ' | b'\t')) {
                        j += 1;
                    }
                    let mut delimiter = Vec::new();
                    let word_start = j;
                    while j < b.len() {
                        let d = b[j];
                        if is_space(d) || b";|&<>()".contains(&d) {
                            break;
                        }
                        if d == b'\'' || d == b'"' {
                            let mut k = j + 1;
                            let mut closed = false;
                            while k < b.len() {
                                if d == b'"' && b[k] == b'\\' && k + 1 < b.len() {
                                    delimiter.push(b[k + 1]);
                                    k += 2;
                                    continue;
                                }
                                if b[k] == d {
                                    closed = true;
                                    break;
                                }
                                delimiter.push(b[k]);
                                k += 1;
                            }
                            parsed &= closed;
                            j = k + 1;
                            continue;
                        }
                        if d == b'\\' {
                            if let Some(&e) = b.get(j + 1) {
                                delimiter.push(e);
                            }
                            j += 2;
                            continue;
                        }
                        delimiter.push(d);
                        j += 1;
                    }
                    if j == word_start {
                        parsed = false;
                    }
                    pending.push((delimiter, strip));
                    i = j.min(b.len());
                } else if c == b'\n' && !pending.is_empty() {
                    let mut pos = i + 1;
                    for (delimiter, strip) in pending.drain(..) {
                        let body_start = pos;
                        loop {
                            if pos >= b.len() {
                                bodies.push(body_start..b.len());
                                break;
                            }
                            let end = b[pos..]
                                .iter()
                                .position(|&x| x == b'\n')
                                .map_or(b.len(), |k| pos + k);
                            let mut line = &b[pos..end];
                            if strip {
                                while line.first() == Some(&b'\t') {
                                    line = &line[1..];
                                }
                            }
                            if line == delimiter.as_slice() {
                                bodies.push(body_start..end);
                                pos = (end + 1).min(b.len());
                                break;
                            }
                            pos = end + 1;
                        }
                    }
                    i = pos;
                } else {
                    i += 1;
                }
            }
        }
    }
    (bodies, parsed && state == Q::Unquoted)
}

/// Records, for every byte of `command`, its quoting context, word, and
/// whether it lies in a heredoc body, a substitution or an inner script.
pub fn analyze(command: &str) -> Analysis {
    let source = command.as_bytes();
    let mut bytes = vec![ByteContext::default(); source.len()];
    let (bodies, mut parsed) = heredocs(source);
    let mut work = source.to_vec();
    for body in &bodies {
        for k in body.clone() {
            work[k] = b' ';
            bytes[k].heredoc = true;
        }
    }
    let mut words = Vec::new();
    let mut open_cases = 0i64;
    for segment in segment_ranges(&work) {
        let b = &work[segment.clone()];
        let (found, closed) = groups(b);
        parsed &= closed;
        for g in found {
            for k in g.whole {
                bytes[segment.start + k].substitution = true;
            }
        }
        let tokens = tokenize(b, segment.start, Some(&mut bytes));
        let first = words.len();
        for (n, t) in tokens.iter().enumerate() {
            let raw = segment.start + t.raw.start..segment.start + t.raw.end;
            for c in &mut bytes[raw.clone()] {
                c.word = Some(first + n);
            }
            words.push(raw);
        }
        let trimmed = b.iter().position(|&c| !is_space(c)).map(|k| b[k]);
        if trimmed.is_none_or(|c| c == b'#') {
            continue;
        }
        let (body, delta) = clause(b, &tokens, open_cases > 0);
        open_cases = (open_cases + delta).max(0);
        if let Some(body) = body {
            for k in inner_script_words(b, &tokens[body..]) {
                let raw = &tokens[body + k].raw;
                for c in &mut bytes[segment.start + raw.start..segment.start + raw.end] {
                    c.inner_script = true;
                }
            }
        }
    }
    Analysis {
        parsed,
        bytes,
        words,
    }
}

#[cfg(test)]
mod tests;
