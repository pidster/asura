//! Command rewrite planner (SD7 as extended by SO1; flowchart SD-F1). Moves
//! each detected secret into a per-execution variable, or refuses the whole
//! command with a reason that never holds the value, a preview, an offset or
//! a digest. Pure computation over at most one command.
use super::stream::{RawSpan, redact_whole, scan_spans};
use super::{RuleSet, SecretKind, detection};
use crate::shell::split::{Analysis, ByteContext, Quoting, Role, analyze};
use std::collections::HashMap;
use zeroize::Zeroizing;

/// Distinct secret values per command.
pub const MAX_SECRET_VARIABLES: usize = 16;

/// One secret moved out of a command: number 1–16, kind and value. Debug
/// output never shows the value.
pub struct SecretVariable {
    number: u8,
    kind: SecretKind,
    value: Zeroizing<Vec<u8>>,
}
impl SecretVariable {
    pub fn number(&self) -> u8 {
        self.number
    }
    pub fn kind(&self) -> SecretKind {
        self.kind
    }
    pub fn value(&self) -> &[u8] {
        &self.value
    }
    /// The environment variable name, `ASURA_SECRET_<n>`.
    pub fn name(&self) -> String {
        format!("ASURA_SECRET_{}", self.number)
    }
    /// Any number and value, to exercise `ShellRequest` validation.
    #[cfg(test)]
    pub(crate) fn for_test(number: u8, value: &[u8]) -> Self {
        Self {
            number,
            kind: SecretKind::AssignedSecret,
            value: Zeroizing::new(value.to_vec()),
        }
    }
}
impl std::fmt::Debug for SecretVariable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretVariable")
            .field("number", &self.number)
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

/// The command to record and run, and its secret variables (empty when the
/// command held no detected secret, in which case it is unchanged).
#[derive(Debug)]
pub struct Rewrite {
    pub command: String,
    pub variables: Vec<SecretVariable>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RefusalCode {
    PlaceholderNotAvailable,
    UnparsedCommand,
    SecretInHeredoc,
    SecretInSubstitution,
    SecretInInnerScript,
    SecretSingleQuoted,
    SecretAnsiQuoted,
    SecretMixedQuoting,
    SecretWithShellSyntax,
    TooManySecrets,
    ResidualSecret,
    DetectionUnavailable,
    /// J1: the rewritten command is longer than 8,192 bytes (G6).
    RewrittenCommandTooLong,
    /// J1: the working directory holds a detected secret.
    SecretInCwd,
}
impl RefusalCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PlaceholderNotAvailable => "placeholder_not_available",
            Self::UnparsedCommand => "unparsed_command",
            Self::SecretInHeredoc => "secret_in_heredoc",
            Self::SecretInSubstitution => "secret_in_substitution",
            Self::SecretInInnerScript => "secret_in_inner_script",
            Self::SecretSingleQuoted => "secret_single_quoted",
            Self::SecretAnsiQuoted => "secret_ansi_quoted",
            Self::SecretMixedQuoting => "secret_mixed_quoting",
            Self::SecretWithShellSyntax => "secret_with_shell_syntax",
            Self::TooManySecrets => "too_many_secrets",
            Self::ResidualSecret => "residual_secret",
            Self::DetectionUnavailable => "secret_detection_unavailable",
            Self::RewrittenCommandTooLong => "rewritten_command_too_long",
            Self::SecretInCwd => "secret_in_cwd",
        }
    }
    fn advice(self) -> &'static str {
        match self {
            Self::PlaceholderNotAvailable => {
                "it names a secret variable from earlier output, which no new command can use. Enter the value itself"
            }
            Self::UnparsedCommand => "Asura could not parse the command around it",
            Self::SecretInHeredoc => {
                "it is in a heredoc body. Pass it in a double-quoted argument instead"
            }
            Self::SecretInSubstitution => {
                "it is inside a command substitution. Move it to a double-quoted argument"
            }
            Self::SecretInInnerScript => {
                "it is inside a script given to another shell or eval. Pass it as a separate argument"
            }
            Self::SecretSingleQuoted => {
                "it is single-quoted. Use double quotes so Asura can pass it as a variable"
            }
            Self::SecretAnsiQuoted => "it is in $'…' quotes. Use double quotes",
            Self::SecretMixedQuoting => {
                "it spans more than one quoting context. Quote it as one word"
            }
            Self::SecretWithShellSyntax => {
                "the shell would interpret part of it. Quote it with double quotes"
            }
            Self::TooManySecrets => "the command holds more than 16 secrets",
            Self::ResidualSecret => "part of a secret would remain after the rewrite",
            Self::DetectionUnavailable => "secret detection is unavailable",
            Self::RewrittenCommandTooLong => {
                "the command would be longer than 8192 bytes after the rewrite. Shorten it"
            }
            Self::SecretInCwd => {
                "the working directory holds a secret. Use a directory without one"
            }
        }
    }
}

/// Why a command was refused: a code, the rule kind and the 1-based word
/// index. It never holds the value, a masked preview, an offset or a digest.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Refusal {
    pub code: RefusalCode,
    pub kind: Option<SecretKind>,
    pub word: Option<usize>,
}
impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Command refused: ")?;
        match (self.kind, self.word) {
            (Some(kind), Some(word)) => write!(f, "a `{kind}` secret in word {word}: ")?,
            (Some(kind), None) => write!(f, "a `{kind}` secret: ")?,
            (None, Some(word)) => write!(f, "word {word}: ")?,
            (None, None) => {}
        }
        write!(f, "{}.", self.code.advice())
    }
}
impl std::error::Error for Refusal {}
fn refuse(code: RefusalCode, kind: Option<SecretKind>, word: Option<usize>) -> Refusal {
    Refusal { code, kind, word }
}

/// Rewrites `command` with the process rule set; refuses every command with
/// `secret_detection_unavailable` if the rule set failed its self-check.
pub fn rewrite_command(command: &str) -> Result<Rewrite, Refusal> {
    rewrite_with(detection(), command)
}
pub(crate) fn rewrite_with(
    rules: Result<&'static RuleSet, super::SelfCheckError>,
    command: &str,
) -> Result<Rewrite, Refusal> {
    match rules {
        Ok(rules) => rewrite(rules, command),
        Err(_) => Err(refuse(RefusalCode::DetectionUnavailable, None, None)),
    }
}

/// The rewritten command limit: the rewrite must fit the shell command and
/// intent record limits (J1, G6).
pub const MAX_REWRITTEN_COMMAND_BYTES: usize = 8192;

/// The model shell call path (J1): rewrites `command`, refuses a rewrite
/// longer than [`MAX_REWRITTEN_COMMAND_BYTES`] with
/// `rewritten_command_too_long`, and scans `cwd` in whole-buffer mode; a
/// detected span refuses with `secret_in_cwd`. The person path reuses it.
pub fn rewrite_shell_call(command: &str, cwd: &str) -> Result<Rewrite, Refusal> {
    rewrite_shell_call_with(detection(), command, cwd)
}
pub(crate) fn rewrite_shell_call_with(
    rules: Result<&'static RuleSet, super::SelfCheckError>,
    command: &str,
    cwd: &str,
) -> Result<Rewrite, Refusal> {
    let rewrite = rewrite_with(rules.clone(), command)?;
    let rules = rules.map_err(|_| refuse(RefusalCode::DetectionUnavailable, None, None))?;
    let (spans, withheld) = scan_spans(rules, cwd.as_bytes());
    if let Some(span) = spans.first() {
        return Err(refuse(RefusalCode::SecretInCwd, Some(span.kind), None));
    }
    if withheld {
        return Err(refuse(RefusalCode::UnparsedCommand, None, None));
    }
    if rewrite.command.len() > MAX_REWRITTEN_COMMAND_BYTES {
        return Err(refuse(RefusalCode::RewrittenCommandTooLong, None, None));
    }
    Ok(rewrite)
}

/// The command with every detected span replaced by its placeholder, for a
/// display-redacted record of a refused command. Holds no value.
pub fn display_redacted(rules: &'static RuleSet, command: &str) -> String {
    String::from_utf8_lossy(redact_whole(rules, command.as_bytes()).as_bytes()).into_owned()
}

#[derive(Clone, Copy)]
enum Form {
    /// `"$ASURA_SECRET_<n>"`: a whole unquoted word or a span inside one (SO1).
    Unquoted,
    /// `${ASURA_SECRET_<n>}` inside existing double quotes.
    Braced,
}

/// Rewrites one command (design steps 1–9).
pub fn rewrite(rules: &'static RuleSet, command: &str) -> Result<Rewrite, Refusal> {
    let bytes = command.as_bytes();
    if references_placeholder(bytes) {
        return Err(refuse(RefusalCode::PlaceholderNotAvailable, None, None));
    }
    let (spans, withheld) = scan_spans(rules, bytes);
    if withheld {
        return Err(refuse(RefusalCode::UnparsedCommand, None, None));
    }
    if spans.is_empty() {
        return Ok(Rewrite {
            command: command.to_string(),
            variables: Vec::new(),
        });
    }
    let analysis = analyze(command);
    let mut plan = Vec::with_capacity(spans.len());
    for span in &spans {
        let range = span.start as usize..span.end as usize;
        plan.push((range.clone(), classify(&analysis, bytes, range, span.kind)?));
    }
    // Numbers 1..k by first appearance; equal values share a number.
    let mut numbers: HashMap<&[u8], u8> = HashMap::new();
    let mut variables: Vec<SecretVariable> = Vec::new();
    let mut assigned = Vec::with_capacity(plan.len());
    for ((range, _), span) in plan.iter().zip(&spans) {
        let value = &bytes[range.clone()];
        let next = numbers.len() + 1;
        let number = *numbers.entry(value).or_insert_with(|| next as u8);
        if usize::from(number) == next {
            if next > MAX_SECRET_VARIABLES {
                return Err(refuse(RefusalCode::TooManySecrets, None, None));
            }
            variables.push(SecretVariable {
                number,
                kind: span.kind,
                value: Zeroizing::new(value.to_vec()),
            });
        }
        assigned.push(number);
    }
    let mut out = Vec::with_capacity(bytes.len() + 24 * plan.len());
    let mut inserted = Vec::with_capacity(plan.len());
    let mut cursor = 0;
    for ((range, form), number) in plan.iter().zip(&assigned) {
        out.extend_from_slice(&bytes[cursor..range.start]);
        let text = match form {
            Form::Unquoted => format!("\"$ASURA_SECRET_{number}\""),
            Form::Braced => format!("${{ASURA_SECRET_{number}}}"),
        };
        inserted.push(out.len() as u64..(out.len() + text.len()) as u64);
        out.extend_from_slice(text.as_bytes());
        cursor = range.end;
    }
    out.extend_from_slice(&bytes[cursor..]);
    let Ok(rewritten) = String::from_utf8(out) else {
        return Err(refuse(RefusalCode::UnparsedCommand, None, None));
    };
    verify(rules, &rewritten, &inserted)?;
    Ok(Rewrite {
        command: rewritten,
        variables,
    })
}

/// Step 8: rescan the rewritten command. A match that holds any byte outside
/// the inserted text means part of a secret remained; a match wholly inside
/// it is the inserted name, which rules without the placeholder predicate
/// (for example `netrc-password`) still match.
pub(crate) fn verify(
    rules: &'static RuleSet,
    rewritten: &str,
    inserted: &[std::ops::Range<u64>],
) -> Result<(), Refusal> {
    let (again, withheld) = scan_spans(rules, rewritten.as_bytes());
    if withheld {
        return Err(refuse(RefusalCode::UnparsedCommand, None, None));
    }
    match again.iter().find(|s| !within(s, inserted)) {
        Some(residual) => {
            let word = word_index(
                &analyze(rewritten),
                residual.start as usize..residual.end as usize,
            );
            Err(refuse(
                RefusalCode::ResidualSecret,
                Some(residual.kind),
                word,
            ))
        }
        None => Ok(()),
    }
}

fn within(span: &RawSpan, inserted: &[std::ops::Range<u64>]) -> bool {
    inserted
        .iter()
        .any(|r| r.start <= span.start && span.end <= r.end)
}

/// `$ASURA_SECRET_<n>` or `${ASURA_SECRET_<n>}` anywhere in the command.
fn references_placeholder(bytes: &[u8]) -> bool {
    const NAME: &[u8] = b"ASURA_SECRET_";
    (0..bytes.len()).any(|i| {
        let rest = &bytes[i..];
        let after = if rest.starts_with(b"${") {
            &rest[2..]
        } else if rest.starts_with(b"$") {
            &rest[1..]
        } else {
            return false;
        };
        after.starts_with(NAME) && after.get(NAME.len()).is_some_and(u8::is_ascii_digit)
    })
}

/// The 1-based index of the first word that holds a byte of the span;
/// `None` when no span byte is in a word (a heredoc body).
fn word_index(analysis: &Analysis, range: std::ops::Range<usize>) -> Option<usize> {
    analysis
        .bytes
        .get(range)?
        .iter()
        .find_map(|c| c.word)
        .map(|w| w + 1)
}

/// Bytes a plain unquoted span or word must not hold: quoting, expansion,
/// globbing, brace and tilde expansion, and the operators that would end the
/// word or redirect.
fn plain(bytes: &[u8]) -> bool {
    !bytes
        .iter()
        .any(|b| br#"'"\$`*?[{~|&;<>()"#.contains(b) || super::rules::is_ascii_space(*b))
}

/// An unescaped `$` before the span, possibly followed by name characters
/// that reach the span, would make the span part of a parameter expansion.
fn expansion_reaches(bytes: &[u8], ctx: &[ByteContext], start: usize) -> bool {
    let mut j = start;
    while j > 0 && (bytes[j - 1].is_ascii_alphanumeric() || bytes[j - 1] == b'_') {
        j -= 1;
    }
    j > 0
        && bytes[j - 1] == b'$'
        && ctx[j - 1].role == Role::Text
        && matches!(ctx[j - 1].quoting, Quoting::None | Quoting::Double)
}

/// SD-F1 for one span.
fn classify(
    analysis: &Analysis,
    bytes: &[u8],
    range: std::ops::Range<usize>,
    kind: SecretKind,
) -> Result<Form, Refusal> {
    let word = word_index(analysis, range.clone());
    let no = |code| Err(refuse(code, Some(kind), word));
    if !analysis.parsed {
        return no(RefusalCode::UnparsedCommand);
    }
    let ctx = &analysis.bytes[range.clone()];
    if ctx.iter().any(|c| c.heredoc) {
        return no(RefusalCode::SecretInHeredoc);
    }
    if ctx.iter().any(|c| c.substitution) {
        return no(RefusalCode::SecretInSubstitution);
    }
    if ctx.iter().any(|c| c.inner_script) {
        return no(RefusalCode::SecretInInnerScript);
    }
    let structural = ctx
        .iter()
        .any(|c| matches!(c.role, Role::Separator | Role::QuoteMark));
    let mut quotings: Vec<Quoting> = Vec::new();
    for c in ctx
        .iter()
        .filter(|c| !matches!(c.role, Role::Separator | Role::QuoteMark))
    {
        if !quotings.contains(&c.quoting) {
            quotings.push(c.quoting);
        }
    }
    match quotings.as_slice() {
        [Quoting::Single] => return no(RefusalCode::SecretSingleQuoted),
        [Quoting::Ansi] => return no(RefusalCode::SecretAnsiQuoted),
        [_] if !structural => {}
        _ => return no(RefusalCode::SecretMixedQuoting),
    }
    let value = &bytes[range.clone()];
    let escaped = ctx.iter().any(|c| c.role != Role::Text);
    let before_backslash = range.start > 0 && bytes[range.start - 1] == b'\\';
    let expansion = expansion_reaches(bytes, &analysis.bytes, range.start);
    match quotings[0] {
        Quoting::Double => {
            if escaped
                || before_backslash
                || expansion
                || value.iter().any(|b| matches!(b, b'\\' | b'$' | b'`'))
            {
                return no(RefusalCode::SecretWithShellSyntax);
            }
            Ok(Form::Braced)
        }
        _ => {
            let Some(w) = ctx[0].word else {
                return no(RefusalCode::SecretMixedQuoting);
            };
            let whole = analysis.words[w].clone();
            if !plain(value) || escaped || (range.start == whole.start && value[0] == b'#') {
                return no(RefusalCode::SecretWithShellSyntax);
            }
            if range == whole {
                return Ok(Form::Unquoted);
            }
            // SO1: a span inside an unquoted word.
            let word_bytes = &whole;
            let brace = word_bytes
                .clone()
                .any(|k| bytes[k] == b'{' && analysis.bytes[k].quoting == Quoting::None);
            let assignment_name = bytes[whole.clone()]
                .iter()
                .position(|&b| b == b'=')
                .filter(|&eq| {
                    let name = &bytes[whole.start..whole.start + eq];
                    !name.is_empty()
                        && !name[0].is_ascii_digit()
                        && name.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'_')
                })
                .is_some_and(|eq| range.start <= whole.start + eq);
            if before_backslash || brace || expansion || assignment_name {
                return no(RefusalCode::SecretWithShellSyntax);
            }
            Ok(Form::Unquoted)
        }
    }
}
