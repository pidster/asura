//! The intent feature extractor (security-policy.md, PL2 intent extractor
//! and PL2-F2): the tighten-only `command` features of a stored command,
//! built from the splitter's own helpers. Pure and bounded by the command
//! bound; no I/O. Intent never grants (AD-27): a feature can only make a
//! forbid match, so every bound marks the command `unparsed` instead of
//! guessing.
use super::{
    Quoting, Role, SHELLS, Token, VALUE_OPTIONS, analyze, basename, clause, groups, heredocs,
    inner_scripts, is_name_byte, is_space, segment_ranges, tokenize,
};
use std::collections::BTreeSet;

/// Distinct entries kept in `tools` and in `flags`.
pub const MAX_INTENT_SET: usize = 32;
/// The longest tool, flag or subcommand kept.
pub const MAX_INTENT_WORD: usize = 128;
/// Group and inner-script nesting read.
const MAX_DEPTH: usize = 8;
/// Segments read, at every depth together.
const MAX_SEGMENTS: usize = 64;
/// Wrappers whose next command word is a tool too (design list).
const WRAPPERS: [&str; 7] = ["env", "command", "exec", "nohup", "time", "xargs", "sudo"];
/// Interpreters for `pipe_to_interpreter`, besides the shells.
const INTERPRETERS: [&str; 7] = [
    "python",
    "python3",
    "perl",
    "ruby",
    "node",
    "php",
    "osascript",
];

/// The secret facts of the command's rewrite plan.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PlanFacts {
    /// Secret variables the job receives, numbered and named.
    pub secret_vars: u32,
    /// At least one named reference.
    pub secret_reference: bool,
    /// The plan was a whole-command plan.
    pub whole_command: bool,
}

/// The `parse` feature.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Parse {
    Simple,
    Compound,
    Unparsed,
}
impl Parse {
    pub fn as_str(self) -> &'static str {
        match self {
            Parse::Simple => "simple",
            Parse::Compound => "compound",
            Parse::Unparsed => "unparsed",
        }
    }
}

/// The `command` record of an intent (schema type `Command`). No feature
/// holds a word position beyond `first_tool` and `subcommand`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntentFeatures {
    pub parse: Parse,
    /// Distinct tool basenames in order of appearance, at most 32.
    pub tools: Vec<String>,
    pub first_tool: String,
    pub subcommand: String,
    /// Distinct flags of the first segment, cut at their first `=`, in
    /// order of appearance, at most 32.
    pub flags: Vec<String>,
    /// Names from the closed set, sorted.
    pub shell_features: Vec<&'static str>,
    pub bytes: u64,
    pub secret_vars: u32,
    pub secret_reference: bool,
    pub whole_command: bool,
}

#[derive(Default)]
struct Walk {
    unparsed: bool,
    /// A bound was passed: nothing more is read.
    stop: bool,
    compound: bool,
    segments: usize,
    first_seen: bool,
    tools: Vec<String>,
    first_tool: String,
    subcommand: String,
    flags: Vec<String>,
    features: BTreeSet<&'static str>,
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}
fn named(bytes: &[u8], names: &[&str]) -> bool {
    names.iter().any(|n| bytes == n.as_bytes())
}

/// The splitter's assignment rule (`unwrap`).
fn assignment(b: &[u8], t: &Token) -> bool {
    let (raw, value) = (&b[t.raw.clone()], &t.value[..]);
    !raw.starts_with(b"(")
        && !raw.starts_with(b"$(")
        && value.contains(&b'=')
        && !value.starts_with(b"=")
        && value
            .split(|&c| c == b'=')
            .next()
            .is_some_and(|name| name.iter().all(|&c| is_name_byte(c)))
}

/// A redirection word: `(is a redirection, takes the next word as target)`.
fn redirection(raw: &[u8]) -> (bool, bool) {
    let rest = &raw[raw.iter().take_while(|c| c.is_ascii_digit()).count()..];
    let redirect = rest.starts_with(b"<") || rest.starts_with(b">") || rest.starts_with(b"&>");
    // `<(…)` and `>(…)` are process substitutions, not redirections.
    if !redirect || rest.starts_with(b"<(") || rest.starts_with(b">(") {
        return (false, false);
    }
    let operator = rest.iter().take_while(|c| b"<>&|".contains(c)).count();
    (true, operator == rest.len())
}

/// Adds one distinct entry to a bounded set; a bound marks `unparsed`.
fn push_bounded(set: &mut Vec<String>, word: &[u8], unparsed: &mut bool) {
    if word.is_empty() {
        return;
    }
    if word.len() > MAX_INTENT_WORD {
        *unparsed = true;
        return;
    }
    let word = String::from_utf8_lossy(word).into_owned();
    if set.contains(&word) {
        return;
    }
    if set.len() == MAX_INTENT_SET {
        *unparsed = true;
        return;
    }
    set.push(word);
}

impl Walk {
    /// The byte-level shell features of one text (`top`: the stored
    /// command itself, whose operators outside groups count as top level).
    fn scan(&mut self, text: &str, top: bool) {
        let analysis = analyze(text);
        if !analysis.parsed {
            self.unparsed = true;
        }
        let b = text.as_bytes();
        let f = &mut self.features;
        let rest_has_command = |from: usize| {
            b.get(from..)
                .and_then(|r| r.iter().position(|&c| !is_space(c)).map(|k| r[k]))
                .is_some_and(|c| c != b'#')
        };
        let param = |next: Option<u8>| {
            next.is_some_and(|n| is_name_byte(n) || n == b'{' || b"@*#?$!-".contains(&n))
        };
        let mut i = 0;
        while i < b.len() {
            let c = analysis.bytes[i];
            if c.heredoc || matches!(c.role, Role::Escape | Role::Escaped | Role::QuoteMark) {
                i += 1;
                continue;
            }
            let next = b.get(i + 1).copied();
            let prev = i.checked_sub(1).map(|k| b[k]);
            if c.quoting == Quoting::Double {
                match b[i] {
                    b'$' if next == Some(b'(') => {
                        f.insert("command_substitution");
                    }
                    b'$' if param(next) => {
                        f.insert("parameter_expansion");
                    }
                    b'`' => {
                        f.insert("command_substitution");
                    }
                    _ => {}
                }
                i += 1;
                continue;
            }
            if c.quoting != Quoting::None {
                i += 1;
                continue;
            }
            let level = top && !c.substitution;
            let before = b[..i].iter().any(|&x| !is_space(x));
            match b[i] {
                b'|' if next == Some(b'|') => {
                    if level {
                        f.insert("list");
                    }
                    i += 2;
                    continue;
                }
                b'|' if prev == Some(b'>') => {}
                b'|' => {
                    if level {
                        f.insert("pipe");
                    }
                }
                b'&' if next == Some(b'&') => {
                    if level {
                        f.insert("list");
                    }
                    i += 2;
                    continue;
                }
                b'&' if next == Some(b'>') => {
                    f.insert("redirect_out");
                    i += 2;
                    continue;
                }
                b'&' if matches!(prev, Some(b'>' | b'<' | b'|')) => {}
                b'&' => {
                    if level && before {
                        f.insert("background");
                    }
                }
                b';' | b'\n' => {
                    if level && before && rest_has_command(i + 1) {
                        f.insert("list");
                    }
                }
                b'>' | b'<' if next == Some(b'(') => {
                    f.insert("process_substitution");
                }
                b'>' => {
                    f.insert("redirect_out");
                    if next == Some(b'>') {
                        i += 2;
                        continue;
                    }
                }
                b'<' if next == Some(b'<') => {
                    if b.get(i + 2) == Some(&b'<') {
                        f.insert("redirect_in");
                        i += 3;
                    } else {
                        f.insert("heredoc");
                        self.unparsed = true;
                        i += 2;
                    }
                    continue;
                }
                b'<' => {
                    f.insert("redirect_in");
                }
                b'$' if next == Some(b'(') => {
                    f.insert("command_substitution");
                }
                b'$' if param(next) => {
                    f.insert("parameter_expansion");
                }
                b'`' => {
                    f.insert("command_substitution");
                }
                b'(' => {
                    let arithmetic = prev == Some(b'(') && i >= 2 && b[i - 2] == b'$';
                    let substitution = matches!(prev, Some(b'$' | b'<' | b'>'));
                    if !arithmetic && !substitution && next != Some(b')') {
                        f.insert("subshell");
                    }
                }
                b'*' | b'?' | b'[' if prev != Some(b'$') && c.word.is_some() => {
                    f.insert("glob");
                }
                b'~' if c.word.is_some_and(|w| analysis.words[w].start == i) => {
                    f.insert("tilde");
                }
                _ => {}
            }
            i += 1;
        }
    }

    fn tool(&mut self, name: &[u8]) {
        push_bounded(&mut self.tools, name, &mut self.unparsed);
    }

    /// `first_tool`, `subcommand` and `flags` from the first top-level
    /// segment's words, starting at its first command word.
    fn first_segment(&mut self, b: &[u8], words: &[Token]) {
        let name = basename(&words[0].value);
        if name.len() > MAX_INTENT_WORD {
            self.unparsed = true;
        } else {
            self.first_tool = String::from_utf8_lossy(name).into_owned();
        }
        let mut subcommand = None;
        let mut target = false;
        for word in &words[1..] {
            if std::mem::take(&mut target) {
                continue;
            }
            let (redirect, operator_only) = redirection(&b[word.raw.clone()]);
            if redirect {
                target = operator_only;
                continue;
            }
            let value = &word.value[..];
            if value.starts_with(b"-") {
                let cut = value.split(|&c| c == b'=').next().unwrap_or(value);
                push_bounded(&mut self.flags, cut, &mut self.unparsed);
            } else if subcommand.is_none() && !value.is_empty() {
                subcommand = Some(value);
            }
        }
        if let Some(value) = subcommand {
            if value.len() > MAX_INTENT_WORD {
                self.unparsed = true;
            } else {
                self.subcommand = String::from_utf8_lossy(value).into_owned();
            }
        }
    }

    /// Reads the segments of `text`, then, recursively, their group
    /// bodies and inner scripts, within the depth and segment bounds.
    fn walk(&mut self, text: &[u8], depth: usize, top: bool) {
        if self.stop {
            return;
        }
        let (bodies, parsed) = heredocs(text);
        if !parsed {
            self.unparsed = true;
        }
        let mut work = text.to_vec();
        if !bodies.is_empty() {
            self.unparsed = true;
            self.features.insert("heredoc");
            for body in &bodies {
                for k in body.clone() {
                    work[k] = b' ';
                }
            }
        }
        let mut open_cases = 0i64;
        for range in segment_ranges(&work) {
            if self.stop {
                return;
            }
            let seg = &work[range.clone()];
            let Some(start) = seg.iter().position(|&c| !is_space(c)) else {
                continue;
            };
            if seg[start] == b'#' {
                continue;
            }
            self.segments += 1;
            if self.segments > MAX_SEGMENTS {
                self.unparsed = true;
                self.stop = true;
                return;
            }
            if self.segments > 1 {
                self.compound = true;
            }
            let first_top = top && !self.first_seen;
            if top {
                self.first_seen = true;
            }
            let (found, closed) = groups(seg);
            if !closed {
                self.unparsed = true;
            }
            let subshell = seg[start] == b'(' && found.iter().any(|g| g.whole.start == start);
            if !subshell {
                self.command(seg, &work, range.clone(), first_top, &mut open_cases);
            }
            // Group bodies after the command word: tools keep their order of
            // appearance.
            for group in &found {
                self.compound = true;
                if depth + 1 > MAX_DEPTH {
                    self.unparsed = true;
                    self.stop = true;
                    return;
                }
                let body = seg[group.body.clone()].to_vec();
                self.walk(&body, depth + 1, false);
                if self.stop {
                    return;
                }
            }
            if subshell {
                continue;
            }
            let Ok(segment) = std::str::from_utf8(seg) else {
                continue;
            };
            for script in inner_scripts(segment) {
                self.compound = true;
                if depth + 1 > MAX_DEPTH {
                    self.unparsed = true;
                    self.stop = true;
                    return;
                }
                self.scan(&script, false);
                self.walk(script.as_bytes(), depth + 1, false);
                if self.stop {
                    return;
                }
            }
        }
    }

    /// The command words of one segment: tools, the first segment's
    /// features and `pipe_to_interpreter`.
    fn command(
        &mut self,
        seg: &[u8],
        work: &[u8],
        range: std::ops::Range<usize>,
        first_top: bool,
        open_cases: &mut i64,
    ) {
        {
            let tokens = tokenize(seg, 0, None);
            let (body, delta) = clause(seg, &tokens, *open_cases > 0);
            *open_cases = (*open_cases + delta).max(0);
            let Some(mut i) = body else {
                return;
            };
            let mut first_word = None;
            let mut last_tool: Option<Vec<u8>> = None;
            loop {
                while i < tokens.len() && assignment(seg, &tokens[i]) {
                    self.features.insert("assignment");
                    i += 1;
                }
                let Some(word) = tokens.get(i) else {
                    break;
                };
                let raw = &seg[word.raw.clone()];
                if raw.starts_with(b"(") || contains(raw, b"$(") || raw.contains(&b'`') {
                    self.unparsed = true;
                    break;
                }
                let name = basename(&word.value).to_vec();
                self.tool(&name);
                first_word.get_or_insert(i);
                let wrapper = named(&name, &WRAPPERS);
                last_tool = Some(name);
                if !wrapper {
                    break;
                }
                i += 1;
                while let Some(option) = tokens.get(i).map(|t| &t.value[..]) {
                    if !option.starts_with(b"-") {
                        break;
                    }
                    i += 1;
                    if VALUE_OPTIONS.iter().any(|v| option == v.as_bytes()) && i < tokens.len() {
                        i += 1;
                    }
                }
            }
            if first_top && let Some(w) = first_word {
                self.first_segment(seg, &tokens[w..]);
            }
            let piped = range.start > 0
                && work[range.start - 1] == b'|'
                && (range.start < 2 || work[range.start - 2] != b'|');
            if piped
                && last_tool
                    .as_deref()
                    .is_some_and(|t| named(t, SHELLS) || named(t, &INTERPRETERS))
            {
                self.features.insert("pipe_to_interpreter");
            }
        }
    }
}

/// The intent features of one stored (rewritten) command. A whole-command
/// plan reads the stored form and is always `unparsed`.
pub fn intent(command: &str, plan: PlanFacts) -> IntentFeatures {
    let mut walk = Walk {
        unparsed: plan.whole_command,
        ..Walk::default()
    };
    walk.scan(command, true);
    walk.walk(command.as_bytes(), 0, true);
    for tool in &walk.tools {
        let t = tool.as_bytes();
        if t == b"eval" {
            walk.features.insert("eval");
        }
        if t == b"source" || t == b"." {
            walk.features.insert("source");
        }
        if named(t, SHELLS) {
            walk.features.insert("nested_shell");
        }
    }
    let parse = if walk.unparsed {
        Parse::Unparsed
    } else if walk.compound {
        Parse::Compound
    } else {
        Parse::Simple
    };
    IntentFeatures {
        parse,
        tools: walk.tools,
        first_tool: walk.first_tool,
        subcommand: walk.subcommand,
        flags: walk.flags,
        shell_features: walk.features.into_iter().collect(),
        bytes: command.len() as u64,
        secret_vars: plan.secret_vars,
        secret_reference: plan.secret_reference,
        whole_command: plan.whole_command,
    }
}
