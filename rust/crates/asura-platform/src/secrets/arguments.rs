//! Tool-call argument redaction (`docs/designs/tool-call-records.md`,
//! "Argument redaction", TCR-D4 and TCR-D19–D22; `secret-detection.md`,
//! "Tool call argument redaction"). Produces the only stored copy of a model
//! tool call's name and argument text. Pure bounded computation; the only
//! system call is the per-call random key. Never logs.
use super::json::{self, Value};
use super::rewrite::SecretVariable;
use super::stream::scan_spans;
use super::{RuleSet, SecretKind};
use aho_corasick::{AhoCorasick, MatchKind};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::ops::Range;
use zeroize::{Zeroize, Zeroizing};

/// Argument text limit, received and stored.
pub const MAX_ARGUMENT_BYTES: usize = 64_000;
/// Received name limit.
pub const MAX_NAME_BYTES: usize = 256;
/// Stored name limit after redaction.
pub const MAX_STORED_NAME_BYTES: usize = 512;
/// Distinct numbered placeholders per call; later values are unnumbered.
pub const MAX_ARGUMENT_NUMBERS: usize = 256;
/// A string (or name) whose scan withheld bytes (TCR-D21).
pub const WITHHELD_TEXT: &str = "[withheld]";
/// A refused exec_cmd command is cut at this many bytes.
const MAX_REFUSED_COMMAND_BYTES: usize = 8192;
const TOKEN: &[u8] = b"ASURA_SECRET_";

/// How the top-level `command` member of a parsed exec_cmd call is stored.
pub enum ExecCmdCommand<'a> {
    /// SL3 rewrite succeeded: the rewritten command replaces the value
    /// verbatim; its variables keep numbers 1..k and seed the value set.
    Rewritten {
        command: &'a str,
        variables: &'a [SecretVariable],
    },
    /// SL3 refused: the raw value is display-redacted with the shared map,
    /// then cut at 8,192 bytes without splitting a placeholder.
    Refused,
    /// SL7 whole-command variable: the rewrite's display text replaces the
    /// value verbatim, cut like a refused command. Its variables seed the
    /// value set; numbering then follows first appearance (TCR-D22), which
    /// keeps the display's numbers.
    WholeCommand {
        display: &'a str,
        variables: &'a [SecretVariable],
    },
}

pub struct ArgumentsInput<'a> {
    /// `None`: not received (over 256 bytes).
    pub name: Option<&'a str>,
    /// `None`: not received (over 64,000 bytes).
    pub arguments: Option<&'a str>,
    /// Some only when the service's typed exec_cmd parse succeeded; the text
    /// is then stored in canonical form.
    pub exec_cmd: Option<ExecCmdCommand<'a>>,
    /// The name is the registered exec_cmd name: parseable text is always
    /// stored in canonical form.
    pub exec_cmd_name: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotRetained {
    Oversized,
    DetectionUnavailable,
    UnverifiableEscape,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgumentForm {
    Verbatim,
    Canonical,
    Unparsed,
    NotRetained(NotRetained),
}

#[derive(Debug)]
pub struct RedactedArguments {
    /// `None`: not retained (absent, or longer than 512 bytes once redacted).
    pub name: Option<String>,
    pub form: ArgumentForm,
    /// Empty for `NotRetained`.
    pub text: String,
    /// Entry i is the kind of `$ASURA_SECRET_<i+1>`.
    pub number_kinds: Vec<SecretKind>,
    /// Number-0 tokens in the stored name and text.
    pub unnumbered: u32,
    /// A refused or whole-command exec_cmd command was cut at 8,192 bytes.
    pub command_cut: bool,
}

/// Placeholder tokens (TCR-D20) in one text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlaceholderTokens {
    /// Highest nonzero number, 0 when none.
    pub max: u32,
    /// Distinct nonzero numbers, sorted.
    pub distinct: Vec<u32>,
    /// Count of number-0 tokens.
    pub zero: u32,
    /// A token with a leading zero or a number that does not fit `u32`.
    pub malformed: bool,
}

/// One token: byte range of its digits and the parsed number (`None` when
/// malformed).
struct Token {
    digits: Range<usize>,
    number: Option<u32>,
}
/// `ASURA_SECRET_` plus one or more ASCII digits, directly after `$` or `${`.
fn tokens(text: &str) -> Vec<Token> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(found) = find(&bytes[from..], TOKEN) {
        let at = from + found;
        from = at + TOKEN.len();
        let prefixed = (at >= 1 && bytes[at - 1] == b'$')
            || (at >= 2 && bytes[at - 1] == b'{' && bytes[at - 2] == b'$');
        let start = at + TOKEN.len();
        let mut end = start;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        if !prefixed || end == start {
            continue;
        }
        let digits = &text[start..end];
        let number = if digits.len() > 1 && digits.starts_with('0') {
            None
        } else {
            digits.parse::<u32>().ok()
        };
        out.push(Token {
            digits: start..end,
            number,
        });
        from = end;
    }
    out
}
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Counts placeholder tokens (TCR-D20); storage replay uses it.
pub fn placeholder_tokens(text: &str) -> PlaceholderTokens {
    let mut result = PlaceholderTokens::default();
    for token in tokens(text) {
        match token.number {
            None => result.malformed = true,
            Some(0) => result.zero += 1,
            Some(n) => {
                result.max = result.max.max(n);
                if let Err(at) = result.distinct.binary_search(&n) {
                    result.distinct.insert(at, n);
                }
            }
        }
    }
    result
}

/// Model-written tokens become the unnumbered form (TCR-D4).
fn neutralize(out: &mut String, text: &str) {
    let mut cursor = 0;
    for token in tokens(text) {
        out.push_str(&text[cursor..token.digits.start]);
        out.push('0');
        cursor = token.digits.end;
    }
    out.push_str(&text[cursor..]);
}

/// Per-call number map keyed by a random key; holds no value bytes.
struct Numbers {
    key: Zeroizing<[u8; 32]>,
    map: HashMap<[u8; 32], u32>,
    kinds: Vec<SecretKind>,
}
impl Numbers {
    fn new() -> Self {
        let mut key = Zeroizing::new([0u8; 32]);
        crate::fill_random(&mut key[..]);
        Self {
            key,
            map: HashMap::new(),
            kinds: Vec::new(),
        }
    }
    fn digest(&self, value: &[u8]) -> [u8; 32] {
        let mut hash = Sha256::new();
        hash.update(&self.key[..]);
        hash.update(value);
        hash.finalize().into()
    }
    fn lookup(&self, value: &[u8]) -> Option<u32> {
        self.map.get(&self.digest(value)).copied()
    }
    fn assign(&mut self, value: &[u8], kind: SecretKind) -> u32 {
        let digest = self.digest(value);
        if let Some(&n) = self.map.get(&digest) {
            return n;
        }
        if self.kinds.len() >= MAX_ARGUMENT_NUMBERS {
            return 0;
        }
        self.kinds.push(kind);
        let n = self.kinds.len() as u32;
        self.map.insert(digest, n);
        n
    }
}

/// Every value found by any scan (TCR-D19), matched literally everywhere.
#[derive(Default)]
struct Values {
    entries: Vec<(Zeroizing<Vec<u8>>, SecretKind)>,
    seen: std::collections::HashSet<[u8; 32]>,
}
impl Values {
    fn add(&mut self, numbers: &Numbers, value: &[u8], kind: SecretKind) {
        if !value.is_empty() && self.seen.insert(numbers.digest(value)) {
            self.entries.push((Zeroizing::new(value.to_vec()), kind));
        }
    }
    /// Spans of one scan; returns whether the scan withheld bytes.
    fn scan(&mut self, rules: &'static RuleSet, numbers: &Numbers, text: &str) -> bool {
        let (spans, withheld) = scan_spans(rules, text.as_bytes());
        for span in spans {
            let value = &text.as_bytes()[span.start as usize..span.end as usize];
            self.add(numbers, value, span.kind);
        }
        withheld
    }
    fn matcher(&self) -> Option<AhoCorasick> {
        if self.entries.is_empty() {
            return None;
        }
        AhoCorasick::builder()
            .match_kind(MatchKind::Standard)
            .build(self.entries.iter().map(|(v, _)| v.as_slice()))
            .ok()
    }
}

struct Redactor<'a> {
    values: &'a Values,
    matcher: Option<AhoCorasick>,
}
impl Redactor<'_> {
    /// Replaces the union of all value occurrences (merged when they
    /// overlap) and neutralizes model tokens in the rest. Without `assign`, a
    /// region whose bytes have no number makes the result `None`. Returns the
    /// text and the byte ranges of the inserted placeholders.
    fn redact(
        &self,
        text: &str,
        numbers: &mut Numbers,
        assign: bool,
    ) -> Option<(String, Vec<Range<usize>>)> {
        let mut regions: Vec<(Range<usize>, usize)> = Vec::new();
        if let Some(matcher) = &self.matcher {
            for m in matcher.find_overlapping_iter(text.as_bytes()) {
                regions.push((m.start()..m.end(), m.pattern().as_usize()));
            }
        }
        regions.sort_by_key(|(r, _)| (r.start, std::cmp::Reverse(r.end)));
        let mut merged: Vec<(Range<usize>, usize)> = Vec::new();
        for (range, pattern) in regions {
            match merged.last_mut() {
                Some((last, _)) if range.start < last.end => last.end = last.end.max(range.end),
                _ => merged.push((range, pattern)),
            }
        }
        let mut out = String::with_capacity(text.len());
        let mut inserted = Vec::new();
        let mut cursor = 0;
        for (mut range, pattern) in merged {
            if range.start < cursor {
                range.start = cursor;
            }
            while !text.is_char_boundary(range.start) {
                range.start -= 1;
            }
            while !text.is_char_boundary(range.end) {
                range.end += 1;
            }
            if range.start < cursor || range.end <= range.start {
                continue;
            }
            let bytes = &text.as_bytes()[range.clone()];
            let kind = self.values.entries[pattern].1;
            let number = if assign {
                numbers.assign(bytes, kind)
            } else {
                numbers.lookup(bytes)?
            };
            neutralize(&mut out, &text[cursor..range.start]);
            let start = out.len();
            out.push_str(&super::placeholder(number));
            inserted.push(start..out.len());
            cursor = range.end;
        }
        neutralize(&mut out, &text[cursor..]);
        Some((out, inserted))
    }
}

fn wipe(value: &mut Value) {
    match value {
        Value::String(s) | Value::Number(s) => s.zeroize(),
        Value::Array(items) => items.iter_mut().for_each(wipe),
        Value::Object(members) => {
            for (key, item) in members {
                key.zeroize();
                wipe(item);
            }
        }
        Value::Null | Value::Bool(_) => {}
    }
}
/// Zeroizes a parsed tree that held raw model text when dropped.
struct Wiped(Value);
impl Drop for Wiped {
    fn drop(&mut self) {
        wipe(&mut self.0);
    }
}

/// Collects keys and strings in document order; the exec_cmd command member
/// is skipped when it is replaced by the rewrite.
fn collect(
    value: &Value,
    rules: &'static RuleSet,
    numbers: &Numbers,
    values: &mut Values,
    withheld: &mut Vec<bool>,
    skip_command: bool,
    top: bool,
) {
    match value {
        Value::String(s) => withheld.push(values.scan(rules, numbers, s)),
        Value::Array(items) => {
            for item in items {
                collect(item, rules, numbers, values, withheld, false, false);
            }
        }
        Value::Object(members) => {
            for (key, item) in members {
                withheld.push(values.scan(rules, numbers, key));
                if top && skip_command && key == "command" && matches!(item, Value::String(_)) {
                    continue;
                }
                collect(item, rules, numbers, values, withheld, false, false);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

struct Emit<'a, 'b> {
    redactor: &'a Redactor<'b>,
    withheld: std::vec::IntoIter<bool>,
    exec_cmd: Option<&'a ExecCmdCommand<'a>>,
    command_cut: bool,
}
impl Emit<'_, '_> {
    fn string(&mut self, s: &str, numbers: &mut Numbers) -> String {
        if self.withheld.next().unwrap_or(true) {
            return WITHHELD_TEXT.into();
        }
        self.redactor
            .redact(s, numbers, true)
            .map(|(text, _)| text)
            .unwrap_or_else(|| WITHHELD_TEXT.into())
    }
    fn command(&mut self, s: &str, numbers: &mut Numbers) -> String {
        if self.withheld.next().unwrap_or(true) {
            return WITHHELD_TEXT.into();
        }
        let Some((mut text, inserted)) = self.redactor.redact(s, numbers, true) else {
            return WITHHELD_TEXT.into();
        };
        self.command_cut |= cut_command(&mut text, &inserted);
        text
    }
    /// The whole-command display text, verbatim, cut like a refused command.
    fn display(&mut self, display: &str) -> String {
        let mut text = display.to_owned();
        let placeholders = placeholder_ranges(&text);
        self.command_cut |= cut_command(&mut text, &placeholders);
        text
    }
    fn value(&mut self, value: &Value, numbers: &mut Numbers, top: bool) -> Value {
        match value {
            Value::String(s) => Value::String(self.string(s, numbers)),
            Value::Array(items) => Value::Array(
                items
                    .iter()
                    .map(|item| self.value(item, numbers, false))
                    .collect(),
            ),
            Value::Object(members) => {
                let mut out = Vec::with_capacity(members.len());
                for (key, item) in members {
                    let key_out = self.string(key, numbers);
                    let item_out = match (top, key.as_str(), item, self.exec_cmd) {
                        (
                            true,
                            "command",
                            Value::String(_),
                            Some(ExecCmdCommand::Rewritten { command, .. }),
                        ) => Value::String((*command).to_owned()),
                        (true, "command", Value::String(s), Some(ExecCmdCommand::Refused)) => {
                            Value::String(self.command(s, numbers))
                        }
                        (
                            true,
                            "command",
                            Value::String(_),
                            Some(ExecCmdCommand::WholeCommand { display, .. }),
                        ) => Value::String(self.display(display)),
                        _ => self.value(item, numbers, false),
                    };
                    out.push((key_out, item_out));
                }
                Value::Object(out)
            }
            other => other.clone(),
        }
    }
}

/// Cuts a command at 8,192 bytes on a character boundary, never inside a
/// placeholder. Returns whether it cut.
fn cut_command(text: &mut String, placeholders: &[Range<usize>]) -> bool {
    if text.len() <= MAX_REFUSED_COMMAND_BYTES {
        return false;
    }
    let mut cut = MAX_REFUSED_COMMAND_BYTES;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    if let Some(range) = placeholders.iter().find(|r| r.start < cut && cut < r.end) {
        cut = range.start;
    }
    text.truncate(cut);
    true
}
/// Byte ranges of the `$ASURA_SECRET_<n>` and `${ASURA_SECRET_<n>` tokens.
fn placeholder_ranges(text: &str) -> Vec<Range<usize>> {
    let bytes = text.as_bytes();
    tokens(text)
        .into_iter()
        .map(|token| {
            let at = token.digits.start - TOKEN.len();
            let start = if at >= 2 && bytes[at - 1] == b'{' {
                at - 2
            } else {
                at - 1
            };
            let end = if bytes.get(token.digits.end) == Some(&b'}') && start + 2 == at {
                token.digits.end + 1
            } else {
                token.digits.end
            };
            start..end
        })
        .collect()
}

/// Best-effort JSON unescape of unparsed text: invalid escapes are copied.
fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.char_indices().peekable();
    let hex4 = |at: usize| -> Option<u32> {
        let digits = text.get(at..at + 4)?;
        u32::from_str_radix(digits, 16)
            .ok()
            .filter(|_| digits.bytes().all(|b| b.is_ascii_hexdigit()))
    };
    while let Some((at, c)) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let Some(&(_, next)) = chars.peek() else {
            out.push(c);
            break;
        };
        let simple = match next {
            '"' => Some('"'),
            '\\' => Some('\\'),
            '/' => Some('/'),
            'b' => Some('\u{8}'),
            'f' => Some('\u{c}'),
            'n' => Some('\n'),
            'r' => Some('\r'),
            't' => Some('\t'),
            _ => None,
        };
        if let Some(simple) = simple {
            chars.next();
            out.push(simple);
            continue;
        }
        if next == 'u'
            && let Some(high) = hex4(at + 2)
        {
            let mut code = Some(high);
            let mut width = 6;
            if (0xD800..0xDC00).contains(&high) {
                code = None;
                if text.get(at + 6..at + 8) == Some("\\u")
                    && let Some(low) = hex4(at + 8)
                    && (0xDC00..0xE000).contains(&low)
                {
                    code = Some(0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00));
                    width = 12;
                }
            }
            if let Some(decoded) = code.and_then(char::from_u32) {
                out.push(decoded);
                while chars.peek().is_some_and(|(i, _)| *i < at + width) {
                    chars.next();
                }
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// Renumbers in first-appearance order over name then text (TCR-D22);
/// numbers 1..=`fixed` keep their value. Returns the new kinds.
fn renumber(
    name: &mut Option<String>,
    text: &mut String,
    kinds: &[SecretKind],
    fixed: u32,
) -> Vec<SecretKind> {
    let mut map: HashMap<u32, u32> = (1..=fixed).map(|n| (n, n)).collect();
    let mut order: Vec<u32> = (1..=fixed).collect();
    let mut rewrite = |s: &mut String| {
        let mut out = String::with_capacity(s.len());
        let mut cursor = 0;
        for token in tokens(s) {
            let Some(old) = token.number.filter(|n| *n != 0) else {
                continue;
            };
            let next = map.len() as u32 + 1;
            let new = *map.entry(old).or_insert_with(|| {
                order.push(old);
                next
            });
            out.push_str(&s[cursor..token.digits.start]);
            out.push_str(&new.to_string());
            cursor = token.digits.end;
        }
        out.push_str(&s[cursor..]);
        *s = out;
    };
    if let Some(name) = name.as_mut() {
        rewrite(name);
    }
    rewrite(text);
    order.iter().map(|old| kinds[*old as usize - 1]).collect()
}

/// Produces the stored name and argument text of one call. The caller
/// handles a rule set that failed its self-check (decision `refused`).
pub fn redact_arguments(rules: &'static RuleSet, input: &ArgumentsInput<'_>) -> RedactedArguments {
    let mut numbers = Numbers::new();
    let mut values = Values::default();
    let mut fixed = 0;
    match &input.exec_cmd {
        Some(ExecCmdCommand::Rewritten { variables, .. }) => {
            for variable in variables.iter() {
                numbers.assign(variable.value(), variable.kind());
                values.add(&numbers, variable.value(), variable.kind());
            }
            fixed = variables.len() as u32;
        }
        // A cut or withheld display may omit numbers, so none is fixed.
        Some(ExecCmdCommand::WholeCommand { variables, .. }) => {
            for variable in variables.iter() {
                numbers.assign(variable.value(), variable.kind());
                values.add(&numbers, variable.value(), variable.kind());
            }
        }
        _ => {}
    }
    let name_withheld = input
        .name
        .is_some_and(|name| values.scan(rules, &numbers, name));
    let text = input
        .arguments
        .filter(|text| text.len() <= MAX_ARGUMENT_BYTES);
    let raw_withheld = text.is_some_and(|text| values.scan(rules, &numbers, text));
    let parsed = text.map(|text| json::parse(text).map(Wiped));
    let mut withheld = Vec::new();
    if let Some(Ok(tree)) = &parsed {
        let skip = matches!(
            input.exec_cmd,
            Some(ExecCmdCommand::Rewritten { .. } | ExecCmdCommand::WholeCommand { .. })
        );
        collect(
            &tree.0,
            rules,
            &numbers,
            &mut values,
            &mut withheld,
            skip,
            true,
        );
    }
    let redactor = Redactor {
        matcher: values.matcher(),
        values: &values,
    };
    let mut name = input.name.map(|name| {
        if name_withheld {
            WITHHELD_TEXT.to_owned()
        } else {
            redactor
                .redact(name, &mut numbers, true)
                .map_or_else(|| WITHHELD_TEXT.to_owned(), |(text, _)| text)
        }
    });
    let mut command_cut = false;
    let (form, mut stored) = match (text, &parsed) {
        (None, _) | (_, None) => (
            ArgumentForm::NotRetained(NotRetained::Oversized),
            String::new(),
        ),
        (Some(_), Some(Ok(tree))) => {
            let mut emit = Emit {
                redactor: &redactor,
                withheld: withheld.into_iter(),
                exec_cmd: input.exec_cmd.as_ref(),
                command_cut: false,
            };
            let c = Wiped(emit.value(&tree.0, &mut numbers, true));
            command_cut = emit.command_cut;
            let canonical = json::canonical(&c.0);
            if input.exec_cmd.is_some() || input.exec_cmd_name || raw_withheld {
                (ArgumentForm::Canonical, canonical)
            } else {
                let raw = text.unwrap_or_default();
                match redactor.redact(raw, &mut numbers, false) {
                    Some((v, _)) if json::parse(&v).is_ok_and(|p| Wiped(p).0 == c.0) => {
                        (ArgumentForm::Verbatim, v)
                    }
                    _ => (ArgumentForm::Canonical, canonical),
                }
            }
        }
        (Some(raw), Some(Err(_))) => {
            if raw_withheld {
                (
                    ArgumentForm::NotRetained(NotRetained::DetectionUnavailable),
                    String::new(),
                )
            } else {
                let v = redactor
                    .redact(raw, &mut numbers, true)
                    .map(|(v, _)| v)
                    .unwrap_or_default();
                let hidden = raw.contains('\\') && {
                    let copy = Zeroizing::new(unescape(raw));
                    !scan_spans(rules, copy.as_bytes()).0.is_empty()
                };
                if hidden {
                    (
                        ArgumentForm::NotRetained(NotRetained::UnverifiableEscape),
                        String::new(),
                    )
                } else {
                    (ArgumentForm::Unparsed, v)
                }
            }
        }
    };
    let (form, mut stored) = if stored.len() > MAX_ARGUMENT_BYTES {
        stored.zeroize();
        (
            ArgumentForm::NotRetained(NotRetained::Oversized),
            String::new(),
        )
    } else {
        (form, stored)
    };
    if name
        .as_ref()
        .is_some_and(|n| n.len() > MAX_STORED_NAME_BYTES)
    {
        name = None;
    }
    let number_kinds = renumber(&mut name, &mut stored, &numbers.kinds, fixed);
    let unnumbered =
        placeholder_tokens(name.as_deref().unwrap_or("")).zero + placeholder_tokens(&stored).zero;
    RedactedArguments {
        name,
        form,
        text: stored,
        number_kinds,
        unnumbered,
        command_cut,
    }
}

#[cfg(test)]
#[path = "arguments_tests.rs"]
mod tests;
