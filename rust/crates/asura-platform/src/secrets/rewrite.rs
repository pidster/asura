//! Command rewrite planner (SD7 as extended by SO1; flowchart SD-F1). Moves
//! each detected secret into a per-execution variable. A command the rewrite
//! cannot move runs through the whole-command variable (SO5, slice SL7); only
//! the three kept refusals refuse it. A reason never holds the value, a
//! preview, an offset or a digest. Pure computation over at most one command.
use super::stream::{RawSpan, redact_whole, scan_spans};
use super::{RuleSet, SecretKind, detection};
use crate::exec_cmd::split::{Analysis, ByteContext, Quoting, Role, analyze};
use std::collections::HashMap;
use zeroize::Zeroizing;

/// Distinct secret values per command.
pub const MAX_SECRET_VARIABLES: usize = 16;
/// Detected values of a whole-command run that get a number and a capture
/// literal; later values show the unnumbered placeholder (TCR-D4).
pub const MAX_WHOLE_COMMAND_NUMBERS: usize = 256;
/// The whole-command variable's name (SO5).
pub const SECRET_COMMAND_NAME: &str = "ASURA_SECRET_COMMAND";
/// The fixed executable command of a whole-command run (SO5). It is the
/// only command text the argument vector and the records hold for it.
pub const WHOLE_COMMAND: &str = "eval \"$ASURA_SECRET_COMMAND\"";
/// What views and the model's result say beside the display text (SL7).
pub const WHOLE_COMMAND_NOTICE: &str = "command filtered: run from a secret variable";

/// One secret moved out of a command: number 1–16 (1–256 as a whole-command
/// capture literal), kind and value. Debug output never shows the value.
pub struct SecretVariable {
    number: u16,
    kind: SecretKind,
    value: Zeroizing<Vec<u8>>,
}
impl SecretVariable {
    pub fn number(&self) -> u16 {
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
    /// Any number and value, to exercise `ExecCmdRequest` validation.
    #[cfg(test)]
    pub(crate) fn for_test(number: u16, value: &[u8]) -> Self {
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

/// The whole raw command as the value of `ASURA_SECRET_COMMAND` (SO5).
/// Zeroed on drop; debug output never shows it.
pub struct SecretCommand(Zeroizing<Vec<u8>>);
impl SecretCommand {
    /// Takes a copy of `command`. Request validation checks its size.
    pub fn new(command: &[u8]) -> Self {
        Self(Zeroizing::new(command.to_vec()))
    }
    pub fn value(&self) -> &[u8] {
        &self.0
    }
}
impl std::fmt::Debug for SecretCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretCommand")
            .field("bytes", &self.0.len())
            .finish_non_exhaustive()
    }
}

/// A command the rewrite cannot move (SO5): it runs as [`WHOLE_COMMAND`]
/// with the raw text in `ASURA_SECRET_COMMAND`. `reason` is the rewrite's
/// refusal, recorded as the fallback reason. `display` is the raw command
/// with each detected value replaced by its placeholder under the ordinary
/// numbering, or `[withheld]` when that text cannot be verified free of
/// secrets. `variables` are the detected values with those numbers, as
/// capture literals only; they never reach the environment.
#[derive(Debug)]
pub struct WholeCommand {
    pub reason: Refusal,
    pub display: String,
    pub variables: Vec<SecretVariable>,
    pub command: SecretCommand,
}

/// How an admitted exec_cmd command runs.
#[derive(Debug)]
pub enum Plan {
    Rewritten(Rewrite),
    WholeCommand(WholeCommand),
}
impl Plan {
    /// The executable command text: the rewrite, or [`WHOLE_COMMAND`].
    pub fn command(&self) -> &str {
        match self {
            Self::Rewritten(rewrite) => &rewrite.command,
            Self::WholeCommand(_) => WHOLE_COMMAND,
        }
    }
    /// The rewrite, when the plan is one.
    pub fn rewritten(self) -> Option<Rewrite> {
        match self {
            Self::Rewritten(rewrite) => Some(rewrite),
            Self::WholeCommand(_) => None,
        }
    }
    /// The whole-command run, when the plan is one.
    pub fn whole_command(self) -> Option<WholeCommand> {
        match self {
            Self::Rewritten(_) => None,
            Self::WholeCommand(whole) => Some(whole),
        }
    }
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
    /// The refusals SO5 keeps: none is a secret in the command line, so the
    /// whole-command variable cannot help.
    pub fn kept(self) -> bool {
        matches!(
            self,
            Self::PlaceholderNotAvailable | Self::SecretInCwd | Self::DetectionUnavailable
        )
    }
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

/// Plans `command` with the process rule set: the rewrite, or the
/// whole-command variable (SO5). Refuses only with the kept codes;
/// `secret_detection_unavailable` when the rule set failed its self-check.
pub fn rewrite_command(command: &str) -> Result<Plan, Refusal> {
    plan_with(detection(), command)
}
pub(crate) fn plan_with(
    rules: Result<&'static RuleSet, super::SelfCheckError>,
    command: &str,
) -> Result<Plan, Refusal> {
    match rules {
        Ok(rules) => plan(rules, command),
        Err(_) => Err(refuse(RefusalCode::DetectionUnavailable, None, None)),
    }
}

/// The rewritten command limit: the rewrite must fit the exec_cmd command and
/// intent record limits (J1, G6). A longer rewrite runs through the
/// whole-command variable with reason `rewritten_command_too_long`.
pub const MAX_REWRITTEN_COMMAND_BYTES: usize = 8192;

/// The model exec_cmd call path (J1, SL7). The kept refusals come first, in
/// order: detection unavailable, a reserved placeholder in the command, then
/// the whole-buffer scan of `cwd` (a detected span or withheld bytes refuse
/// with `secret_in_cwd`). Then [`plan`]. The person path reuses it.
/// It compiles the rule set on first use, so only worker threads and tests
/// call it; reactor code calls [`rewrite_exec_cmd_call_with`].
pub fn rewrite_exec_cmd_call(command: &str, cwd: &str) -> Result<Plan, Refusal> {
    exec_cmd_call_checked(detection(), command, cwd)
}
/// The call path over a self-check result: a failed self-check refuses with
/// `secret_detection_unavailable` before any other check.
pub(crate) fn exec_cmd_call_checked(
    rules: Result<&'static RuleSet, super::SelfCheckError>,
    command: &str,
    cwd: &str,
) -> Result<Plan, Refusal> {
    match rules {
        Ok(rules) => rewrite_exec_cmd_call_with(rules, command, cwd),
        Err(_) => Err(refuse(RefusalCode::DetectionUnavailable, None, None)),
    }
}
/// [`rewrite_exec_cmd_call`] with a ready rule set, for a reactor caller
/// that read it from [`super::compiled`]. That caller refuses with
/// `secret_detection_unavailable` itself for a failed self-check or a set
/// that is not ready. This function never compiles.
pub fn rewrite_exec_cmd_call_with(
    rules: &'static RuleSet,
    command: &str,
    cwd: &str,
) -> Result<Plan, Refusal> {
    if references_placeholder(command.as_bytes()) {
        return Err(refuse(RefusalCode::PlaceholderNotAvailable, None, None));
    }
    let (spans, withheld) = scan_spans(rules, cwd.as_bytes());
    if let Some(span) = spans.first() {
        return Err(refuse(RefusalCode::SecretInCwd, Some(span.kind), None));
    }
    if withheld {
        return Err(refuse(RefusalCode::SecretInCwd, None, None));
    }
    plan(rules, command)
}

/// SO5: the rewrite when it succeeds within [`MAX_REWRITTEN_COMMAND_BYTES`];
/// otherwise the whole-command variable, with the rewrite's refusal (or
/// `rewritten_command_too_long`) as its reason. A kept refusal refuses.
pub fn plan(rules: &'static RuleSet, command: &str) -> Result<Plan, Refusal> {
    match rewrite(rules, command) {
        Ok(rewrite) if rewrite.command.len() <= MAX_REWRITTEN_COMMAND_BYTES => {
            Ok(Plan::Rewritten(rewrite))
        }
        Ok(_) => Ok(Plan::WholeCommand(whole_command(
            rules,
            command,
            refuse(RefusalCode::RewrittenCommandTooLong, None, None),
        ))),
        Err(refusal) if refusal.code.kept() => Err(refusal),
        Err(refusal) => Ok(Plan::WholeCommand(whole_command(rules, command, refusal))),
    }
}

/// The whole-command variable for `command` (SO5): numbers 1..k by first
/// appearance, equal values sharing one, at most
/// [`MAX_WHOLE_COMMAND_NUMBERS`]; the display text replaces each detected
/// span with its placeholder and is rescanned like a rewrite (step 8). A
/// withheld scan, invalid UTF-8 or a residual match gives `[withheld]`.
fn whole_command(rules: &'static RuleSet, command: &str, reason: Refusal) -> WholeCommand {
    let bytes = command.as_bytes();
    let (spans, withheld) = scan_spans(rules, bytes);
    let mut numbers: HashMap<&[u8], u16> = HashMap::new();
    let mut variables: Vec<SecretVariable> = Vec::new();
    let mut out = Vec::with_capacity(bytes.len() + 16 * spans.len());
    let mut inserted = Vec::with_capacity(spans.len());
    let mut cursor = 0;
    for span in &spans {
        let range = span.start as usize..span.end as usize;
        let value = &bytes[range.clone()];
        let number = match numbers.get(value) {
            Some(&number) => number,
            None if numbers.len() < MAX_WHOLE_COMMAND_NUMBERS => {
                let number = numbers.len() as u16 + 1;
                numbers.insert(value, number);
                variables.push(SecretVariable {
                    number,
                    kind: span.kind,
                    value: Zeroizing::new(value.to_vec()),
                });
                number
            }
            None => 0,
        };
        out.extend_from_slice(&bytes[cursor..range.start]);
        let text = super::placeholder(u32::from(number));
        inserted.push(out.len() as u64..(out.len() + text.len()) as u64);
        out.extend_from_slice(text.as_bytes());
        cursor = range.end;
    }
    out.extend_from_slice(&bytes[cursor..]);
    let display = match String::from_utf8(out) {
        Ok(display) if !withheld && verify(rules, &display, &inserted).is_ok() => display,
        Ok(mut display) => {
            zeroize::Zeroize::zeroize(&mut display);
            super::WITHHELD_TEXT.to_owned()
        }
        Err(error) => {
            let mut raw = error.into_bytes();
            zeroize::Zeroize::zeroize(&mut raw);
            super::WITHHELD_TEXT.to_owned()
        }
    };
    WholeCommand {
        reason,
        display,
        variables,
        command: SecretCommand::new(bytes),
    }
}

/// The command with every detected span replaced by its placeholder, for a
/// display-redacted record of a refused command. Holds no value.
pub fn display_redacted(rules: &'static RuleSet, command: &str) -> String {
    String::from_utf8_lossy(redact_whole(rules, command.as_bytes()).as_bytes()).into_owned()
}

/// A person note with each detected span replaced by the unnumbered
/// placeholder `$ASURA_SECRET_0` (secret-detection.md, person note
/// redaction). A whole-buffer scan, as in [`display_redacted`]. It assigns
/// no numbers, so it needs no number map and no key. Placeholders the person
/// typed, numbered or braced, stay as typed: the not-placeholder predicate
/// keeps most rules from matching one, and a span that is exactly a
/// placeholder token (a rule without that predicate, such as SR-42 after
/// `htpasswd -b`) is kept as well. Withheld bytes (a line over 64 KiB,
/// which the 2,048-byte note limit rules out) or a span that splits a
/// character give `[withheld]`, so the result never holds a value.
pub fn note_redacted(rules: &'static RuleSet, text: &str) -> String {
    let bytes = text.as_bytes();
    let (spans, withheld) = scan_spans(rules, bytes);
    if withheld {
        return super::WITHHELD_TEXT.to_owned();
    }
    let unnumbered = super::placeholder(0);
    let mut out = Zeroizing::new(Vec::with_capacity(
        bytes.len() + unnumbered.len() * spans.len(),
    ));
    let mut cursor = 0;
    for span in &spans {
        let range = span.start as usize..span.end as usize;
        if is_placeholder_token(&bytes[range.clone()]) {
            continue;
        }
        out.extend_from_slice(&bytes[cursor..range.start]);
        out.extend_from_slice(unnumbered.as_bytes());
        cursor = range.end;
    }
    out.extend_from_slice(&bytes[cursor..]);
    match std::str::from_utf8(&out) {
        Ok(redacted) => redacted.to_owned(),
        Err(_) => super::WITHHELD_TEXT.to_owned(),
    }
}

#[derive(Clone, Copy)]
enum Form {
    /// `"$ASURA_SECRET_<n>"`: a whole unquoted word or a span inside one (SO1).
    Unquoted,
    /// `${ASURA_SECRET_<n>}` inside existing double quotes.
    Braced,
}

/// Rewrites one command (design steps 1–9; flowchart SD-F1). [`plan`]
/// turns its refusals into the whole-command variable.
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
    let mut numbers: HashMap<&[u8], u16> = HashMap::new();
    let mut variables: Vec<SecretVariable> = Vec::new();
    let mut assigned = Vec::with_capacity(plan.len());
    for ((range, _), span) in plan.iter().zip(&spans) {
        let value = &bytes[range.clone()];
        let next = numbers.len() + 1;
        let number = *numbers.entry(value).or_insert_with(|| next as u16);
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
/// Whether `value` is exactly one placeholder token, `$ASURA_SECRET_<n>` or
/// `${ASURA_SECRET_<n>}`, with at least one digit.
pub(crate) fn is_placeholder_token(value: &[u8]) -> bool {
    let digits = if let Some(rest) = value.strip_prefix(b"${ASURA_SECRET_") {
        rest.strip_suffix(b"}")
    } else {
        value.strip_prefix(b"$ASURA_SECRET_")
    };
    digits.is_some_and(|d| !d.is_empty() && d.iter().all(u8::is_ascii_digit))
}

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
