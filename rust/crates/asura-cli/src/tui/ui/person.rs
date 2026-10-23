//! Person command mode (person-command-mode.md): the composer's message, `!`
//! and `!!` modes, the command display, the submit request, refusal texts,
//! the log entry and its fold, the stop routing of Ctrl-C and `/cancel`, and
//! the `/status` secret store lines (secret-store.md, operations and audit).
//! Rendering and key handling perform no I/O; the job worker fetches folds
//! and the queue worker sends submits.
use super::*;
use crate::tui::jobs::{self as worker, Command as JobCommand, FoldTarget, FoldUpdate};
use asura_control::{pb, person as rules};

// Composer roles (person-command-mode.md PCR5, PCR22, PCR26; composer
// interactions, hidden characters). Defined once here.
/// `!` box and strips; command amber.
pub(super) const COMMAND_BOX: Color = Color::Rgb(0xE8, 0xB5, 0x77);
/// `!!` box and strips; outside red.
pub(super) const OUTSIDE_BOX: Color = Color::Rgb(0xD2, 0x70, 0x68);
/// A sent `!` command's stripe, and the `!` display's caret and rule.
pub(super) const COMMAND_SENT: Color = Color::Rgb(0x74, 0x5A, 0x3C);
/// A sent `!!` command's stripe, and the `!!` display's caret and rule.
pub(super) const OUTSIDE_SENT: Color = Color::Rgb(0x69, 0x38, 0x34);
/// The PCR22 warning colour.
pub(super) const ALERT: Color = Color::Rgb(0xFF, 0x6B, 0x6B);
/// Marker and text in both command modes.
pub(super) const MARKER_BLACK: Color = Color::Rgb(0x00, 0x00, 0x00);
/// The hidden-character mark in message and `!` modes.
pub(super) const HIDDEN_MARK: Color = Color::Rgb(0xB3, 0x26, 0x1E);
/// Light stripe text.
pub(super) const STRIPE_TEXT: Color = Color::Rgb(0xE6, 0xED, 0xF0);

/// The PCR22 warning, drawn when a `!!` row is staged.
pub(super) const OUTSIDE_WARNING: &str =
    "running OUTSIDE the sandbox, as you typed it: it may write anywhere you can";
pub(super) const TYPE_A_COMMAND: &str = "Type a command";
pub(super) const SELECT_PROJECT: &str = "Select a project to run commands";
/// Lines of retained output the log fold shows (PCR15).
pub(super) const FOLD_LINES: usize = 8;
/// Person log entries kept for the log; older ones leave the view.
pub(super) const MAX_LOG_ENTRIES: usize = 16;

/// The composer's mode beside the editor draft. The editor text never holds
/// the leading `!` or `!!`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Mode {
    #[default]
    Message,
    Command,
    Outside,
}
impl Mode {
    pub(super) fn command(self) -> bool {
        self != Self::Message
    }
    pub(super) fn marker(self) -> &'static str {
        match self {
            Self::Message => "›",
            Self::Command => "!",
            Self::Outside => "!!",
        }
    }
    fn up(self) -> Option<Self> {
        match self {
            Self::Message => Some(Self::Command),
            Self::Command => Some(Self::Outside),
            Self::Outside => None,
        }
    }
    fn down(self) -> Option<Self> {
        match self {
            Self::Message => None,
            Self::Command => Some(Self::Message),
            Self::Outside => Some(Self::Command),
        }
    }
    pub(super) fn tint(self) -> Option<Color> {
        match self {
            Self::Message => None,
            Self::Command => Some(COMMAND_BOX),
            Self::Outside => Some(OUTSIDE_BOX),
        }
    }
    fn sent(self) -> Color {
        if self == Self::Outside {
            OUTSIDE_SENT
        } else {
            COMMAND_SENT
        }
    }
}

// Recall stores the text with its mode (PCR16). The editor never accepts
// control characters, so a control prefix cannot collide with a message.
const RECALL_COMMAND: char = '\u{1}';
const RECALL_OUTSIDE: char = '\u{2}';
/// The recall entry for a draft and its mode.
pub(super) fn recall_entry(mode: Mode, text: &str) -> String {
    match mode {
        Mode::Message => text.to_owned(),
        Mode::Command => format!("{RECALL_COMMAND}{text}"),
        Mode::Outside => format!("{RECALL_OUTSIDE}{text}"),
    }
}
/// The mode and text of a recall entry.
pub(super) fn recalled(entry: &str) -> (Mode, &str) {
    if let Some(text) = entry.strip_prefix(RECALL_COMMAND) {
        (Mode::Command, text)
    } else if let Some(text) = entry.strip_prefix(RECALL_OUTSIDE) {
        (Mode::Outside, text)
    } else {
        (Mode::Message, entry)
    }
}

/// The cached split of a command draft (PX5). `starts` holds the byte offset
/// of each top-level command; the first is 0. Unparseable text is one block.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Split {
    pub parsed: bool,
    pub starts: Vec<usize>,
    /// Hidden characters in the draft (composer interactions).
    pub hidden: usize,
}
impl Split {
    pub(super) fn commands(&self) -> usize {
        self.starts.len().max(1)
    }
}

/// A hidden character: C0 and C1 controls other than newline and tab, and the
/// listed format characters (composer-interactions.md, hidden characters).
pub(super) fn hidden(c: char) -> bool {
    (c.is_control() && c != '\n' && c != '\t')
        || matches!(c,
            '\u{00AD}' | '\u{180E}' | '\u{200B}'..='\u{200F}' | '\u{2028}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}')
}

/// Splits a command draft into top-level commands with the one canonical
/// splitter (`asura_platform::exec_cmd::split`). A top-level command ends at
/// an unquoted newline, `;` or `&` outside any substitution or subshell; a
/// chain joined by `&&`, `||` or `|` is one command. An unclosed quote,
/// substitution or group, any heredoc, or text over the command bound is not
/// split: the splitter never guesses.
pub(super) fn split(text: &str) -> Split {
    use asura_platform::exec_cmd::split::{Quoting, Role, analyze};
    let hidden = text.chars().filter(|c| hidden(*c)).count();
    let unsplit = Split {
        parsed: false,
        starts: vec![0],
        hidden,
    };
    if text.len() > rules::MAX_COMMAND_BYTES {
        return unsplit;
    }
    let analysis = analyze(text);
    if !analysis.parsed || analysis.bytes.iter().any(|b| b.heredoc) {
        return unsplit;
    }
    let bytes = text.as_bytes();
    let mut starts = vec![0];
    let mut pending: Option<usize> = None;
    for (i, &b) in bytes.iter().enumerate() {
        if let Some(start) = pending
            && !matches!(b, b' ' | b'\t' | b'\n' | b';' | b'&')
        {
            // A segment holding only blanks or separators starts nothing.
            if start > *starts.last().unwrap() {
                starts.push(i);
            }
            pending = None;
        }
        let context = &analysis.bytes[i];
        let top = context.quoting == Quoting::None
            && context.role == Role::Separator
            && !context.substitution
            && !context.heredoc;
        if !top || !matches!(b, b';' | b'\n' | b'&') {
            continue;
        }
        // A backslash-newline continues the command.
        // (The analysis drops the pair as a separator; an odd run of
        // backslashes before the newline escapes it.)
        if b == b'\n' && bytes[..i].iter().rev().take_while(|&&c| c == b'\\').count() % 2 == 1 {
            continue;
        }
        if b == b'&' {
            let before = i.checked_sub(1).map(|k| bytes[k]);
            let after = bytes.get(i + 1).copied();
            // `&&` chains, and `>&`, `&>` and `|&` redirect.
            if matches!(before, Some(b'&' | b'>' | b'|')) || matches!(after, Some(b'&' | b'>')) {
                continue;
            }
        }
        if pending.is_none() {
            pending = Some(i + 1);
        }
    }
    Split {
        parsed: true,
        starts,
        hidden,
    }
}

/// One display row of the command view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Gutter {
    /// The first command's first row: the `!` or `!!` marker.
    Marker,
    /// A later command's first row: the caret `›`.
    Caret,
    /// A continuation row.
    Blank,
    /// The thin rule between two commands; it has no text.
    Rule,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Cell {
    pub text: String,
    pub byte: usize,
    pub width: usize,
    pub hidden: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Row {
    pub gutter: Gutter,
    pub cells: Vec<Cell>,
    /// The byte just past this row's last cell.
    pub end: usize,
}
/// Lays a command draft out for the command view: each top-level command
/// starts a row, a rule row separates commands, and long lines wrap at
/// `width` cells. A hidden character takes one cell.
pub(super) fn layout(text: &str, split: &Split, width: usize) -> Vec<Row> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut bounds = split.starts.clone();
    if bounds.first() != Some(&0) {
        bounds.insert(0, 0);
    }
    bounds.push(text.len());
    for (index, window) in bounds.windows(2).enumerate() {
        let (start, end) = (window[0], window[1].max(window[0]));
        if index > 0 {
            rows.push(Row {
                gutter: Gutter::Rule,
                cells: Vec::new(),
                end: start,
            });
        }
        let mut row = Row {
            gutter: if index == 0 {
                Gutter::Marker
            } else {
                Gutter::Caret
            },
            cells: Vec::new(),
            end: start,
        };
        let mut used = 0;
        for (offset, grapheme) in text[start..end].grapheme_indices(true) {
            let byte = start + offset;
            if grapheme == "\n" {
                // A newline inside a command continues on a blank-gutter
                // row; a separating newline ends the command's last row.
                let next = Row {
                    gutter: Gutter::Blank,
                    cells: Vec::new(),
                    end: byte + 1,
                };
                row.end = byte;
                if byte + 1 < end {
                    rows.push(std::mem::replace(&mut row, next));
                    used = 0;
                }
                continue;
            }
            let is_hidden = grapheme.chars().all(hidden)
                || (grapheme.chars().count() == 1 && grapheme.chars().any(hidden));
            let (shown, cells) = if is_hidden {
                ("█".to_owned(), 1)
            } else {
                (
                    grapheme.to_owned(),
                    unicode_display_width::width(grapheme) as usize,
                )
            };
            if used + cells > width && !row.cells.is_empty() {
                let next = Row {
                    gutter: Gutter::Blank,
                    cells: Vec::new(),
                    end: byte,
                };
                rows.push(std::mem::replace(&mut row, next));
                used = 0;
            }
            row.cells.push(Cell {
                text: shown,
                byte,
                width: cells,
                hidden: is_hidden,
            });
            used += cells;
            row.end = byte + grapheme.len();
        }
        rows.push(row);
    }
    rows
}
/// The cursor's row and cell column in a layout.
pub(super) fn cursor_cell(rows: &[Row], cursor: usize) -> (usize, usize) {
    for (index, row) in rows.iter().enumerate() {
        let mut x = 0;
        for cell in &row.cells {
            if cell.byte == cursor {
                return (index, x);
            }
            x += cell.width;
        }
    }
    let mut last = (0, 0);
    for (index, row) in rows.iter().enumerate() {
        if row.gutter != Gutter::Rule && row.end <= cursor {
            last = (index, row.cells.iter().map(|c| c.width).sum());
        }
    }
    last
}

/// The TUI's environment, read once at launch, in memory only (PC2, PCR21).
#[derive(Clone, Debug, Default)]
pub(super) struct Launch {
    pub path: Option<Vec<u8>>,
    pub environment: Vec<(Vec<u8>, Vec<u8>)>,
}
impl Launch {
    pub(super) fn from_process() -> Self {
        use std::os::unix::ffi::OsStrExt;
        Self {
            path: std::env::var_os("PATH").map(|p| p.as_bytes().to_vec()),
            environment: std::env::vars_os()
                .map(|(name, value)| (name.as_bytes().to_vec(), value.as_bytes().to_vec()))
                .collect(),
        }
    }
    /// The PC3 client variables of a `!` command: only listed names, as set.
    /// The codec rule refuses a bad value; a value that is not UTF-8 cannot
    /// be sent and refuses here with the same reason.
    pub(super) fn client_variables(&self) -> Result<Vec<pb::PersonEnvironment>, pb::QueueRefusal> {
        let mut out = Vec::new();
        for name in rules::CLIENT_VARIABLES {
            if let Some((_, value)) = self
                .environment
                .iter()
                .find(|(key, _)| key.as_slice() == name.as_bytes())
            {
                let value = String::from_utf8(value.clone())
                    .map_err(|_| pb::QueueRefusal::EnvironmentInvalid)?;
                out.push(pb::PersonEnvironment {
                    name: Some(name.into()),
                    value: Some(value),
                });
            }
        }
        Ok(out)
    }
    /// The whole launch environment of a `!!` command, unchanged (PCR21).
    pub(super) fn launch_variables(&self) -> Vec<pb::LaunchVariable> {
        self.environment
            .iter()
            .map(|(name, value)| pb::LaunchVariable {
                name: Some(name.clone()),
                value: Some(value.clone()),
            })
            .collect()
    }
}

/// PC2: the client PATH a `!` submit carries. Empty and relative entries are
/// dropped, keeping order; NUL, more than 4,096 bytes, more than 64 entries,
/// text that is not UTF-8, or no remaining entry is `path_invalid`.
pub(super) fn search_path(raw: Option<&[u8]>) -> Result<String, pb::QueueRefusal> {
    let invalid = pb::QueueRefusal::PathInvalid;
    let raw = raw.ok_or(invalid)?;
    if raw.len() > rules::MAX_SEARCH_PATH_BYTES || raw.contains(&0) {
        return Err(invalid);
    }
    let text = std::str::from_utf8(raw).map_err(|_| invalid)?;
    if text.split(':').count() > 64 {
        return Err(invalid);
    }
    let kept: Vec<&str> = text.split(':').filter(|e| e.starts_with('/')).collect();
    if kept.is_empty() {
        return Err(invalid);
    }
    Ok(kept.join(":"))
}

/// The person-facing text of a person command refusal (control API
/// additions, TUI texts for refusals). `stranded` adds the `/status` hint to
/// `secret_store_full` when stranded items exist.
pub(super) fn refusal_text(
    refusal: pb::QueueRefusal,
    status: Option<u32>,
    stranded: bool,
) -> String {
    use pb::QueueRefusal as R;
    match refusal {
        R::OutsideSandboxOff => "Running outside the sandbox is turned off (personCommands.allowOutsideSandbox is false). Type it after a single ! to run it in the sandbox.".into(),
        R::EnvironmentInvalid => "This terminal's environment cannot be passed to a command (invalid or reserved variable name).".into(),
        R::EnvironmentTooLarge => "This terminal's environment is too large to pass to a command (at most 1,024 variables and 40 KiB).".into(),
        R::PathInvalid => "PATH has no usable absolute entry.".into(),
        R::CwdInvalid => "The working directory cannot be used for commands.".into(),
        R::SecretStoreLocked => "The login keychain is locked. Unlock it, then press Enter again.".into(),
        R::SecretStoreDenied => "macOS denied Asura access to its Keychain item.".into(),
        R::SecretStoreUnavailable => format!(
            "The Keychain is unavailable (OSStatus -{}). Press Enter again later.",
            status.unwrap_or(0)
        ),
        R::SecretStoreFull => format!(
            "The Keychain holds the most Asura items (256); wait for queued commands to finish.{}",
            if stranded { " /status lists Keychain items to delete." } else { "" }
        ),
        R::SecretStoreBusy => "The Keychain is busy. Press Enter again.".into(),
        R::SecretInCwd => "Command refused: the working directory holds a secret.".into(),
        R::PlaceholderNotAvailable => "Command refused: it names a reserved $ASURA_SECRET_ placeholder.".into(),
        R::SecretDetectionUnavailable => "Command refused: secret detection is unavailable in this service.".into(),
        R::Unspecified => "Command refused.".into(),
    }
}

/// `/status` lines for the service's secret store (secret-store.md,
/// operations and audit). They read the last observation only.
pub(super) fn secret_store_lines(status: Option<&pb::SecretStoreStatus>) -> Vec<String> {
    let Some(status) = status else {
        return vec!["Keychain: not reported by this service".into()];
    };
    let mut lines = vec![match status.item_count {
        None => "Keychain items: not known until the first sweep".into(),
        Some(n) if n >= 257 => "more than 256 Keychain items".into(),
        Some(1) => "1 Keychain item".into(),
        Some(n) => format!("{n} Keychain items"),
    }];
    if status.sweep_pending == Some(true) {
        lines.push("Keychain sweep pending".into());
    }
    if let Some(code) = status.last_status.filter(|c| *c != 0) {
        lines.push(format!("Keychain unavailable: last OSStatus -{code}"));
    }
    let stranded = status.stranded_count.unwrap_or(0);
    if stranded > 0 {
        lines.push(format!(
            "Asura cannot delete {stranded} old Keychain item{}. In Keychain Access, delete the login keychain items named 'Asura queued command secrets' with these accounts:",
            if stranded == 1 { "" } else { "s" }
        ));
        for input in status.stranded_inputs.iter().take(16) {
            lines.push(format!("  input.{}", crate::conversation::hex(input)));
        }
        let listed = status.stranded_inputs.len().min(16) as u32;
        if stranded > listed {
            lines.push(format!("  and {} more", stranded - listed));
        }
    }
    let unrecognized = status.unrecognized_count.unwrap_or(0);
    if unrecognized > 0 {
        lines.push(format!(
            "{unrecognized} Keychain item{} under Asura's service name {} not written by Asura.",
            if unrecognized == 1 { "" } else { "s" },
            if unrecognized == 1 { "was" } else { "were" }
        ));
    }
    lines
}

/// What a person command log entry knows. Display state only: every fact
/// about the job comes from the job API.
#[derive(Clone, Debug, Default)]
pub(super) struct LogEntry {
    /// The staged local row's request ID, until the receipt.
    pub request_id: Option<[u8; 16]>,
    pub project: Option<[u8; 16]>,
    pub input: Option<[u8; 16]>,
    pub job: Option<[u8; 16]>,
    pub outside: bool,
    /// The command as staged, or the job's first line when restored.
    pub command: Option<String>,
    /// The display directory: project-relative, or the project's name.
    pub directory: Option<String>,
    /// Transcript entries shown before this entry.
    pub after: usize,
    pub entry: Option<pb::JobEntry>,
    /// The cached end page of stdout and stderr.
    pub pages: [Option<pb::JobStreamPage>; 2],
    /// The fold has the settled job's end pages, or no job exists yet.
    pub fold_done: bool,
    /// The fold read failed or found no job; shown under the entry.
    pub fold_note: Option<String>,
    /// The queue outcome is unknown (the submit wait ended without a reply).
    pub unconfirmed: bool,
    /// The fold read once more after `not_found` while the queue showed the
    /// command started.
    pub fold_retried: bool,
    /// The person's stop request: `Some(false)` stop, `Some(true)` kill.
    pub stop: Option<bool>,
    /// The path warnings shown before the command (queue entry fields 21
    /// and 22, or `JobDetail.path_warnings` and `path_overflow` (field 7)
    /// when restored).
    pub path_warnings: Vec<pb::PathWarning>,
    pub path_overflow: u32,
    /// Held with `paths_changed`: Enter runs it, Esc drops it.
    pub paths_held: bool,
    /// The queue entry waits with `path_check`.
    pub path_check: bool,
    /// The queue dropped the entry before it ran.
    pub dropped: bool,
}
impl LogEntry {
    pub(super) fn state(&self) -> Option<pb::JobState> {
        self.entry
            .as_ref()
            .and_then(|e| e.state)
            .and_then(|s| pb::JobState::try_from(s).ok())
    }
    pub(super) fn settled(&self) -> bool {
        self.state() == Some(pb::JobState::Settled)
    }
    /// A `!!` job whose shell exited while a survivor holds its output (OD2).
    pub(super) fn released(&self) -> bool {
        !self.settled() && self.entry.as_ref().and_then(|e| e.shell_exited) == Some(true)
    }
    fn mode(&self) -> Mode {
        if self.outside {
            Mode::Outside
        } else {
            Mode::Command
        }
    }
    fn first_line(&self) -> (String, usize) {
        let text = self
            .command
            .clone()
            .or_else(|| self.entry.as_ref().and_then(|e| e.command.clone()))
            .unwrap_or_else(|| "…".into());
        let mut lines = text.split('\n');
        let first = lines.next().unwrap_or_default().to_owned();
        (first, lines.count())
    }
    /// A short command excerpt for the status row.
    fn excerpt(&self) -> String {
        shorten(&literal(&self.first_line().0, 512), 32)
    }
}

/// The stripe header of an entry: `! <first line>` with `(+N lines)`.
pub(super) fn header(entry: &LogEntry) -> String {
    let (first, more) = entry.first_line();
    format!(
        "{} {}{}",
        entry.mode().marker(),
        literal(&first, 8192),
        if more > 0 {
            format!(" (+{more} line{})", if more == 1 { "" } else { "s" })
        } else {
            String::new()
        }
    )
}

/// The fold body: the last 8 lines, stdout first, then stderr under a
/// `stderr` label, and the count of folded lines.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Fold {
    /// (is a stderr line, text); the label is not included.
    pub lines: Vec<(bool, String)>,
    pub more: u64,
    /// Lines before the cached pages are not counted.
    pub more_unknown: bool,
}
pub(super) fn fold(entry: &LogEntry) -> Fold {
    let stdout: Vec<String> = entry.pages[0]
        .as_ref()
        .map(super::job_view::page_lines)
        .unwrap_or_default();
    let stderr: Vec<String> = entry.pages[1]
        .as_ref()
        .map(super::job_view::page_lines)
        .unwrap_or_default();
    let cached = (stdout.len() + stderr.len()) as u64;
    let combined: Vec<(bool, String)> = stdout
        .into_iter()
        .map(|l| (false, l))
        .chain(stderr.into_iter().map(|l| (true, l)))
        .collect();
    let skip = combined.len().saturating_sub(FOLD_LINES);
    let lines: Vec<_> = combined.into_iter().skip(skip).collect();
    let shown = lines.len() as u64;
    let counted = entry
        .entry
        .as_ref()
        .and_then(|e| entry.settled().then_some(e.stdout_lines? + e.stderr_lines?));
    let partial = entry
        .pages
        .iter()
        .flatten()
        .any(|p| p.start.unwrap_or(0) > 0);
    let (total, more_unknown) = match counted {
        Some(total) => (total.max(shown), false),
        None => (cached, partial),
    };
    Fold {
        lines,
        more: total - shown,
        more_unknown,
    }
}

fn seconds(ms: u64) -> String {
    if ms < 60_000 {
        format!("{:.1} s", ms as f64 / 1000.0)
    } else {
        format!("{} min {} s", ms / 60_000, ms / 1000 % 60)
    }
}
fn processes(n: u32) -> String {
    format!("{n} background process{}", if n == 1 { "" } else { "es" })
}

/// The footer of a settled or released entry, or the live state.
pub(super) fn footer(entry: &LogEntry) -> Option<String> {
    if entry.unconfirmed {
        return Some("↳ queue outcome unconfirmed; /queue shows it if it was queued".into());
    }
    let job = entry.entry.as_ref()?;
    let left = job.left_running.unwrap_or(0);
    let exit = match (job.exit_kind, job.exit_value) {
        (Some(1), Some(value)) => Some(format!("exit {value}")),
        (Some(2), Some(value)) => Some(format!("signal {value}")),
        _ => None,
    };
    if entry.released() {
        // The shell's status, when the entry carries it while draining.
        let shell = exit.map_or_else(
            || "shell exited".to_owned(),
            |e| format!("shell exited {}", e.replace("exit ", "")),
        );
        return Some(if left > 0 {
            format!("↳ {shell} · {} writing", processes(left))
        } else {
            format!("↳ {shell} · output still open")
        });
    }
    if !entry.settled() {
        return Some(format!("↳ {}", worker::state_text(job)));
    }
    let reason = job.reason.unwrap_or(0);
    let signal = |default: u32| {
        let value = match (job.exit_kind, job.exit_value) {
            (Some(2), Some(value)) => value,
            _ => default,
        };
        format!("signal {value}")
    };
    let mut parts: Vec<String> = Vec::new();
    match job.outcome {
        Some(1 | 2) => {
            parts.push(exit.unwrap_or_else(|| "exited".into()));
            if reason == 4 && left > 0 {
                parts.push("output capture stopped by you".into());
            }
        }
        Some(3) => parts.push(match reason {
            4 => format!("stopped by you ({})", signal(15)),
            5 => format!("killed by you ({})", signal(9)),
            7 => "stopped: service shutdown".into(),
            other => format!("stopped: {}", worker::reason_name(other)),
        }),
        Some(4) => parts.push(format!("not started: {}", worker::reason_name(reason))),
        Some(5) => parts.push("interrupted: service restarted; effects unknown".into()),
        _ => parts.push(worker::state_text(job)),
    }
    if matches!(job.outcome, Some(1..=3)) {
        let lines = job.stdout_lines.unwrap_or(0) + job.stderr_lines.unwrap_or(0);
        parts.push(format!("{lines} line{}", if lines == 1 { "" } else { "s" }));
        if let Some(ms) = job.elapsed_ms {
            parts.push(seconds(ms));
        }
        if let Some(directory) = &entry.directory {
            parts.push(literal(directory, 1024));
        }
    }
    if left > 0 {
        parts.push(format!("{} left running", processes(left)));
    }
    Some(format!("↳ {}", parts.join(" · ")))
}

/// A note on the kept output, as the job viewer gives it: a capture that
/// stopped early, or expired or deleted output.
fn output_note(entry: &LogEntry) -> Option<String> {
    let mut notes = Vec::new();
    for page in entry.pages.iter().flatten() {
        let name = if page.stream == Some(pb::JobStream::Stderr as i32) {
            "stderr"
        } else {
            "stdout"
        };
        match page.state.and_then(|s| pb::JobPageState::try_from(s).ok()) {
            Some(pb::JobPageState::Incomplete) => notes.push(format!(
                "{name}: output kept: first {}",
                worker::bytes(page.total_bytes.unwrap_or(0))
            )),
            Some(pb::JobPageState::Expired) => notes.push(format!("{name}: output expired")),
            Some(pb::JobPageState::Deleted) => notes.push(format!("{name}: output deleted")),
            _ => {}
        }
    }
    (!notes.is_empty()).then(|| notes.join(" · "))
}

/// The styled rows of one log entry: stripe, `!!` warning, fold and footer.
pub(super) fn entry_rows(entry: &LogEntry, text: Style) -> Vec<(String, Style)> {
    let mode = entry.mode();
    let muted = text.fg(Color::Rgb(139, 161, 177));
    let mut rows = vec![(
        header(entry),
        Style::default().bg(mode.sent()).fg(STRIPE_TEXT),
    )];
    if entry.outside {
        rows.push((
            OUTSIDE_WARNING.into(),
            text.fg(ALERT).add_modifier(Modifier::BOLD),
        ));
    }
    // Path warnings, between the header and the output (EPG-R5).
    for line in super::path_guard::warning_lines(&entry.path_warnings, entry.path_overflow) {
        rows.push((line, text.fg(ALERT)));
    }
    if entry.paths_held {
        rows.push((
            super::path_guard::HELD_TEXT.into(),
            text.fg(ALERT).add_modifier(Modifier::BOLD),
        ));
    }
    if entry.dropped {
        rows.push((super::path_guard::DROPPED_TEXT.into(), muted));
        return rows;
    }
    let fold = fold(entry);
    if fold.more > 0 || fold.more_unknown {
        rows.push((
            format!(
                "▸ {}{} more line{}",
                fold.more,
                if fold.more_unknown { "+" } else { "" },
                if fold.more == 1 && !fold.more_unknown {
                    ""
                } else {
                    "s"
                }
            ),
            muted,
        ));
    }
    let mut labelled = false;
    for (stderr, line) in fold.lines {
        if stderr && !labelled {
            rows.push(("stderr".into(), muted.add_modifier(Modifier::ITALIC)));
            labelled = true;
        }
        rows.push((line, text));
    }
    if let Some(note) = output_note(entry) {
        rows.push((note, muted));
    }
    if let Some(note) = entry.fold_note.as_ref().filter(|_| !entry.paths_held) {
        rows.push((format!("↳ {note}"), muted));
    }
    if let Some(footer) = footer(entry) {
        rows.push((footer, muted));
    }
    rows
}

/// The stop target and request of Ctrl-C or `/cancel` (PCM-F2).
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Stop {
    /// No person job to stop: the existing rule applies.
    None,
    /// `JobCancel` for the job of log entry `index`, with kill.
    Send { index: usize, kill: bool },
    /// No new request; the notice repeats the state.
    Repeat(String),
}

/// The one queue entry kind for a person command (control API additions).
pub(super) fn command_entry(entry: &pb::ConversationQueueEntry) -> bool {
    entry.kind == Some(rules::ENTRY_KIND)
}
/// The queue panel row text of a kind 3 entry: `! <excerpt>` or `!! <excerpt>`.
pub(super) fn queue_text(entry: &pb::ConversationQueueEntry) -> String {
    let marker = if entry.command_outside_sandbox == Some(true) {
        "!!"
    } else {
        "!"
    };
    let waiting = entry
        .waiting_text
        .as_deref()
        .map(|t| format!(" · {}", literal(t, rules::MAX_WAITING_TEXT_BYTES)))
        .unwrap_or_default();
    format!(
        "{marker} {}{waiting}",
        literal(entry.text.as_deref().unwrap_or_default(), 256)
    )
}
/// The queue notice of a kind 3 entry.
pub(super) fn queue_notice(entry: &pb::ConversationQueueEntry) -> String {
    match (entry.state, entry.waiting_text.as_deref()) {
        (Some(1), Some(text)) => format!("Command {}", literal(text, 128)),
        (Some(1), None) => "Command queued".into(),
        (Some(2), _) => format!(
            "Command held ({}); Resume or Drop required",
            literal(
                entry.hold_reason.as_deref().unwrap_or("reason unavailable"),
                128
            )
        ),
        (Some(3), _) => "Command running".into(),
        _ => format!("Command {}", queue_state(entry.state).to_lowercase()),
    }
}
/// The job of a person input (job identity: origin 2 and the input ID).
pub(super) fn job_of(input: [u8; 16]) -> [u8; 16] {
    asura_storage::authority::conversation::job_id(
        &asura_storage::authority::conversation::JobOrigin::Person { input },
    )
}

impl App {
    // Service facts.

    /// Capability 8 for the current epoch, from the job worker's status
    /// attachment, and the last observed secret store status.
    pub(in super::super) fn set_person_service(
        &mut self,
        supported: Option<bool>,
        secret_store: Option<pb::SecretStoreStatus>,
    ) {
        // A contended or not yet attached status slot reports nothing; an
        // epoch change clears the fact (`jobs_service_changed`).
        if supported.is_some() {
            self.person.supported = supported;
        }
        if secret_store.is_some() {
            self.person.secret_store = secret_store;
        }
    }
    pub(in super::super) fn service_epoch(&self) -> Option<&str> {
        self.view.epoch.as_deref()
    }
    /// `/status`: the header facts, then the secret store lines. It reads
    /// the last observation only and makes no service request.
    pub(super) fn status_text(&self) -> String {
        let mut lines = vec![
            self.status_header(),
            format!("Lifecycle {}", self.view.lifecycle),
            format!("Installation {}", self.view.installation),
        ];
        if !self.view.reason.is_empty() {
            lines.push(format!("Reason {}", literal(self.view.reason, 128)));
        }
        if let Some(epoch) = &self.view.epoch {
            lines.push(format!("Epoch {epoch}"));
        }
        lines.push(String::new());
        lines.extend(secret_store_lines(self.person.secret_store.as_ref()));
        lines.join("\n")
    }

    // Modes.

    fn person_mode_change(&mut self, mode: Mode) {
        self.mode = mode;
        self.history.reset_navigation();
    }
    /// The mode keys: `!` and Backspace at position 0 with no selection
    /// (PCR1, PCR3, PCR4, PCR20). True when the key changed the mode.
    pub(super) fn person_key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        if self.focus != Focus::Editor
            || self.picker.is_some()
            || self.help.is_some()
            || self.response_selection.is_some()
            || self.history_focus
            || !self.editor.at_start()
        {
            return false;
        }
        let plain = key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT;
        match key.code {
            KeyCode::Char('!') if plain => match self.mode.up() {
                Some(mode) => {
                    self.person_mode_change(mode);
                    true
                }
                None => false,
            },
            KeyCode::Backspace if key.modifiers.is_empty() => match self.mode.down() {
                Some(mode) => {
                    self.person_mode_change(mode);
                    true
                }
                None => false,
            },
            _ => false,
        }
    }
    /// A paste at position 0 steps up one mode for each leading `!`, at most
    /// to outside mode (PCR2, PCR20). Returns the text left to insert.
    pub(super) fn person_paste<'a>(&mut self, text: &'a str) -> &'a str {
        if !self.editor.at_start() || self.help.is_some() {
            return text;
        }
        let mut rest = text;
        while let Some(next) = rest.strip_prefix('!') {
            let Some(mode) = self.mode.up() else { break };
            self.person_mode_change(mode);
            rest = next;
        }
        rest
    }

    // Staging and submit.

    /// The project-relative working directory (PCR9) and its display: the
    /// launch directory when inside the project, else the root.
    pub(super) fn person_directory(&self) -> Option<(String, String)> {
        let project = self.selected_project?;
        let root = self.project_row(project)?.location.clone()?;
        let path = self.context_directory()?;
        let relative = std::path::Path::new(path)
            .strip_prefix(&root)
            .ok()?
            .to_str()?
            .to_owned();
        Some(if relative.is_empty() {
            (".".into(), self.project_name(project))
        } else {
            (relative.clone(), relative)
        })
    }
    /// Enter in command or outside mode: stage the command in the local
    /// outbox, or keep the draft and mode with a notice.
    pub(super) fn submit_command(&mut self) {
        let text = self.editor.text();
        if text.trim().is_empty() {
            self.notice = TYPE_A_COMMAND.into();
            return;
        }
        let Some(project) = self.selected_project else {
            self.notice = SELECT_PROJECT.into();
            return;
        };
        if self.person.supported != Some(true) {
            self.notice = crate::tui::queue::COMMANDS_NOT_ENABLED.into();
            return;
        }
        if text.len() > rules::MAX_COMMAND_BYTES {
            self.notice = "A command is at most 8,192 bytes; draft retained".into();
            return;
        }
        let Some((cwd, directory)) = self
            .person_directory()
            .filter(|(cwd, _)| rules::cwd_form_valid(cwd))
        else {
            self.notice = refusal_text(pb::QueueRefusal::CwdInvalid, None, false);
            return;
        };
        let mode = self.mode;
        let environment = if mode == Mode::Command {
            match (
                search_path(self.person.launch.path.as_deref()),
                self.person.launch.client_variables(),
            ) {
                (Ok(path), Ok(variables)) => Some((path, variables)),
                (Err(refusal), _) | (_, Err(refusal)) => {
                    self.notice = refusal_text(refusal, None, false);
                    return;
                }
            }
        } else {
            None
        };
        let recall = recall_entry(mode, &text);
        // The existing outbox stages the row: its limits, restoration rule
        // and serial sender apply to commands as to messages.
        if !self.stage_local_input(text.clone(), None) {
            return;
        }
        let request_id = self.local_outbox.last().unwrap().request_id;
        self.person.rows.retain(|row| {
            self.local_outbox
                .iter()
                .any(|item| item.request_id == row.request_id)
        });
        self.person.rows.push(CommandRow {
            request_id,
            mode,
            cwd,
            environment,
        });
        self.history.record(&recall);
        self.person.log.push(LogEntry {
            request_id: Some(request_id),
            project: Some(project),
            outside: mode == Mode::Outside,
            command: Some(text),
            directory: Some(directory),
            after: self.transcript.len(),
            ..Default::default()
        });
        if self.person.log.len() > MAX_LOG_ENTRIES {
            self.person.log.remove(0);
        }
        // A successful stage returns the composer to message mode.
        self.mode = Mode::Message;
        self.notice = "Pending local command; waiting for service receipt".into();
    }
    pub(super) fn command_row(&self, request_id: [u8; 16]) -> Option<&CommandRow> {
        self.person
            .rows
            .iter()
            .find(|row| row.request_id == request_id)
    }
    /// The mode of a local outbox row: message unless it is a command.
    pub(super) fn row_mode(&self, item: &LocalInput) -> Mode {
        self.command_row(item.request_id)
            .map_or(Mode::Message, |row| row.mode)
    }
    /// The submit request of a staged command row.
    pub(super) fn command_request(
        &self,
        item: &LocalInput,
        row: &CommandRow,
    ) -> pb::ConversationQueueCommandSubmit {
        let outside = row.mode == Mode::Outside;
        let (search_path, environment) = match &row.environment {
            Some((path, variables)) => (Some(path.clone()), variables.clone()),
            None => (None, Vec::new()),
        };
        pb::ConversationQueueCommandSubmit {
            request_id: Some(item.request_id.to_vec()),
            project_id: Some(item.project.to_vec()),
            conversation_id: self.conversation_id.clone(),
            expected_generation: Some(self.conversation_generation),
            new_conversation: Some(self.conversation_id.is_none()),
            command: Some(item.text.clone()),
            cwd: Some(row.cwd.clone()),
            search_path,
            environment,
            outside_sandbox: outside.then_some(true),
            launch_environment: if outside {
                self.person.launch.launch_variables()
            } else {
                Vec::new()
            },
        }
    }
    /// A typed refusal: the row's text returns to the editor in its mode
    /// when the editor is empty, else the row stays as a rejected row.
    pub(super) fn command_refused(&mut self, index: usize, reply: &pb::ConversationQueueReply) {
        let refusal = reply
            .refusal
            .and_then(|r| pb::QueueRefusal::try_from(r).ok())
            .unwrap_or(pb::QueueRefusal::Unspecified);
        let stranded = self
            .person
            .secret_store
            .as_ref()
            .and_then(|s| s.stranded_count)
            .is_some_and(|n| n > 0);
        let text = refusal_text(refusal, reply.refusal_status, stranded);
        let request = self.local_outbox[index].request_id;
        self.person
            .log
            .retain(|entry| entry.request_id != Some(request));
        self.local_outbox[index].state = LocalInputState::Rejected;
        self.person_restore_rejected(index);
        self.notice = text;
    }
    /// Restores a rejected row into an empty editor with its mode.
    pub(super) fn person_restore_rejected(&mut self, index: usize) -> bool {
        if !self.editor.is_empty() || self.mode != Mode::Message {
            return false;
        }
        let row = &self.local_outbox[index];
        let (text, mode) = (row.text.clone(), self.row_mode(row));
        if self.editor.insert(&text).is_err() {
            return false;
        }
        self.mode = mode;
        let request = self.local_outbox.remove(index).request_id;
        self.person
            .log
            .retain(|entry| entry.request_id != Some(request));
        self.person.rows.retain(|row| row.request_id != request);
        true
    }
    /// The receipt of a staged command: the log entry learns its input and
    /// job, derived from the input ID, and shows the command as the service
    /// stored it (`stored`, the accepted entry's text): a secret in the typed
    /// text is then shown only as its placeholder (PCR19).
    pub(super) fn command_accepted(
        &mut self,
        request: [u8; 16],
        input: &[u8],
        stored: Option<&str>,
    ) {
        let Ok(input) = <[u8; 16]>::try_from(input) else {
            return;
        };
        if let Some(entry) = self
            .person
            .log
            .iter_mut()
            .find(|e| e.request_id == Some(request))
        {
            entry.request_id = None;
            entry.input = Some(input);
            entry.job = Some(job_of(input));
            if let Some(text) = stored.filter(|t| !t.is_empty()) {
                entry.command = Some(text.to_owned());
            }
        }
    }
    /// A submit whose outcome is unknown keeps its entry, marked.
    pub(super) fn command_unconfirmed(&mut self, request: [u8; 16]) {
        if let Some(entry) = self
            .person
            .log
            .iter_mut()
            .find(|e| e.request_id == Some(request))
        {
            entry.unconfirmed = true;
        }
    }
    /// Kind 3 entries of the selected conversation that this client did not
    /// stage get a log entry once they run; a queue state lets a waiting
    /// fold try again.
    pub(super) fn person_queue_seen(&mut self) {
        let project = self.selected_project;
        for entry in &self.queue_entries {
            if !command_entry(entry)
                || entry.conversation_id != self.conversation_id
                || entry.conversation_id.is_none()
            {
                continue;
            }
            let Some(input) = entry
                .input_id
                .as_deref()
                .and_then(|id| <[u8; 16]>::try_from(id).ok())
            else {
                continue;
            };
            let started = matches!(entry.state, Some(3..=5));
            let held = super::path_guard::paths_held(entry);
            if let Some(known) = self.person.log.iter_mut().find(|e| e.input == Some(input)) {
                if started && known.fold_done && !known.settled() {
                    known.fold_done = false;
                    known.fold_note = None;
                }
                super::path_guard::queue_facts(known, entry);
            } else if started || held {
                let mut log = LogEntry {
                    project,
                    input: Some(input),
                    job: Some(job_of(input)),
                    outside: entry.command_outside_sandbox == Some(true),
                    command: entry.text.clone(),
                    directory: entry.command_cwd.clone(),
                    after: self.transcript.len(),
                    ..Default::default()
                };
                super::path_guard::queue_facts(&mut log, entry);
                self.person.log.push(log);
                if self.person.log.len() > MAX_LOG_ENTRIES {
                    self.person.log.remove(0);
                }
            }
        }
    }
    /// Restored person entries (history field 5) in transcript order.
    pub(super) fn person_restored(&mut self, inputs: Vec<(usize, [u8; 16])>) {
        self.person.log.clear();
        let project = self.selected_project;
        for (after, input) in inputs {
            self.person.log.push(LogEntry {
                project,
                input: Some(input),
                job: Some(job_of(input)),
                after,
                ..Default::default()
            });
        }
        let excess = self.person.log.len().saturating_sub(MAX_LOG_ENTRIES);
        self.person.log.drain(..excess);
    }
    /// The transcript dropped its oldest entry.
    pub(super) fn person_transcript_shifted(&mut self) {
        for entry in &mut self.person.log {
            entry.after = entry.after.saturating_sub(1);
        }
    }

    // Folds.

    /// The folds to keep: entries with a job whose fold is not done, newest
    /// first, at most the controller's service live limit.
    pub(in super::super) fn person_folds(&self) -> Vec<FoldTarget> {
        let Some(epoch) = self
            .view
            .epoch
            .clone()
            .filter(|_| self.view.connection == "connected")
        else {
            return Vec::new();
        };
        if self.jobs.supported == Some(false) {
            return Vec::new();
        }
        self.person
            .log
            .iter()
            .rev()
            .filter(|e| !e.fold_done)
            .filter_map(|e| {
                Some(FoldTarget {
                    epoch: epoch.clone(),
                    job: e.job?,
                    project: e.project,
                })
            })
            .take(worker::MAX_FOLDS)
            .collect()
    }
    /// One fold reply replaces the cached fold (PC6).
    pub(in super::super) fn person_folded(&mut self, update: FoldUpdate) -> bool {
        let Some(index) = self
            .person
            .log
            .iter()
            .position(|e| e.job == Some(update.job))
        else {
            return false;
        };
        // The queue's warnings came first when this client saw the entry;
        // a restored entry has only the job detail's.
        if let Some((warnings, overflow)) = update.path_warnings {
            let entry = &mut self.person.log[index];
            if entry.path_warnings.is_empty() && entry.path_overflow == 0 {
                entry.path_warnings = warnings;
                entry.path_overflow = overflow;
            }
        }
        match update.result {
            Ok(page) => {
                if let Some(error) = page.error.as_deref() {
                    let not_found = error == asura_control::jobs::error::NOT_FOUND;
                    let started = self.queue_started(self.person.log[index].input);
                    let entry = &mut self.person.log[index];
                    // The queue already shows the command started: its job
                    // exists now, so the fold tries once more.
                    if not_found && started && !entry.fold_retried {
                        entry.fold_retried = true;
                        return false;
                    }
                    entry.fold_done = true;
                    entry.fold_note = Some(if error == asura_control::jobs::error::NOT_FOUND {
                        "waiting for its turn in the queue".into()
                    } else {
                        worker::error_text(error)
                    });
                    return true;
                }
                if let Some(job) = &page.entry {
                    worker::remember(&mut self.jobs.cache, job);
                }
                let entry = &mut self.person.log[index];
                if let Some(job) = page.entry {
                    entry.outside = job.outside_sandbox == Some(true) || entry.outside;
                    entry.entry = Some(job);
                }
                for page in page.pages {
                    let slot = if page.stream == Some(pb::JobStream::Stderr as i32) {
                        1
                    } else {
                        0
                    };
                    entry.pages[slot] = Some(page);
                }
                entry.fold_note = None;
                entry.fold_done = entry.settled();
            }
            Err(failure) => {
                let entry = &mut self.person.log[index];
                entry.fold_done = true;
                entry.fold_note = Some(match failure {
                    worker::Failure::Unavailable(reason) => {
                        format!("output view unavailable: {}", literal(&reason, 256))
                    }
                    worker::Failure::Unsupported => "this service does not support jobs".into(),
                    _ => "output view stopped".into(),
                });
            }
        }
        true
    }
    /// A job entry from a cancel reply or the viewer reaches the log too.
    pub(super) fn person_entry_changed(&mut self, job: &pb::JobEntry) {
        let id = worker::entry_id(job);
        for entry in &mut self.person.log {
            if entry.job.is_some() && entry.job == id {
                entry.entry = Some(job.clone());
            }
        }
    }
    /// Enter or Space in log focus: the job viewer at the end of stdout for
    /// the lowest person entry in view (PC11).
    pub(super) fn person_open_from_log(&mut self) -> bool {
        let target = self.person_log_target();
        let Some(job) = target
            .and_then(|i| self.person.log.get(i))
            .and_then(|e| e.job)
        else {
            return false;
        };
        let fence = self.person.log[target.unwrap()].project;
        self.open_viewer_at_end(job, fence);
        true
    }

    /// The log entry that log-focus keys act on: the lowest person entry
    /// in view.
    pub(super) fn person_log_target(&self) -> Option<usize> {
        let top = self.history_scroll.min(self.history_scroll_max);
        let bottom = top + self.history_page;
        self.person
            .drawn
            .iter()
            .filter(|(_, row)| *row < bottom)
            .max_by_key(|(_, row)| *row)
            .map(|(index, _)| *index)
    }

    // Stop routing (PCM-F2).

    fn model_turn_active(&self) -> bool {
        self.conversation_busy && self.conversation_cancellable && self.active_operation.is_some()
    }
    /// The queue shows the command dispatched (Running, Complete or Failed).
    fn queue_started(&self, input: Option<[u8; 16]>) -> bool {
        self.queue_entries.iter().any(|q| {
            command_entry(q)
                && matches!(q.state, Some(3..=5))
                && input.is_some_and(|i| q.input_id.as_deref() == Some(i.as_slice()))
        })
    }
    fn queue_running(&self, input: Option<[u8; 16]>) -> bool {
        self.queue_entries.iter().any(|q| {
            command_entry(q)
                && q.state == Some(3)
                && input.is_some_and(|i| q.input_id.as_deref() == Some(i.as_slice()))
        })
    }
    /// The lane's running person job, else the latest released `!!` job.
    fn stop_target(&self) -> Option<usize> {
        let project = self.selected_project;
        let current = |e: &LogEntry| e.job.is_some() && e.project == project && !e.settled();
        let lane = self.person.log.iter().rposition(|e| {
            current(e) && !e.released() && (e.entry.is_some() || self.queue_running(e.input))
        });
        lane.or_else(|| {
            self.person
                .log
                .iter()
                .rposition(|e| current(e) && e.released())
        })
    }
    pub(super) fn person_stop_action(&self, ctrl_c: bool) -> Stop {
        if self.model_turn_active() {
            return Stop::None;
        }
        let Some(index) = self.stop_target() else {
            return Stop::None;
        };
        let entry = &self.person.log[index];
        use pb::JobState as S;
        let stopping = entry.stop.is_some()
            || matches!(
                entry.state(),
                Some(S::Stopping | S::CleanupUnconfirmed | S::Finalizing)
            );
        if !stopping {
            Stop::Send { index, kill: false }
        } else if ctrl_c {
            Stop::Send { index, kill: true }
        } else {
            Stop::Repeat("stopping · Ctrl-C again kills".into())
        }
    }
    /// Ctrl-C or `/cancel` for a person job. False leaves the key to its
    /// existing rule.
    pub(super) fn person_stop(&mut self, ctrl_c: bool) -> bool {
        match self.person_stop_action(ctrl_c) {
            Stop::None => false,
            Stop::Repeat(text) => {
                self.notice = text;
                true
            }
            Stop::Send { index, kill } => {
                let entry = &mut self.person.log[index];
                let (Some(job), Some(project)) = (entry.job, entry.project) else {
                    return false;
                };
                entry.stop = Some(kill || entry.stop == Some(true));
                let command = JobCommand::Cancel {
                    request_id: asura_platform::random_id(),
                    job,
                    project,
                    kill,
                };
                if !self.job_send(command.clone(), None, false) {
                    // The slot is busy: the request waits for it.
                    self.person.deferred = Some(command);
                }
                self.notice = if kill {
                    "killing · KILL sent".into()
                } else {
                    "stopping · Ctrl-C again kills".into()
                };
                true
            }
        }
    }
    /// A stop that ended unconfirmed can be repeated as a stop.
    pub(super) fn person_stop_unconfirmed(&mut self, job: [u8; 16], kill: bool) {
        if kill {
            return;
        }
        for entry in &mut self.person.log {
            if entry.job == Some(job) && entry.stop == Some(false) {
                entry.stop = None;
            }
        }
    }
    /// The status row text of the stop target (running, paused, stopping or
    /// released); none while a model turn runs or nothing runs.
    pub(super) fn person_status(&self, now_unix_ms: u64) -> Option<String> {
        if self.model_turn_active() {
            return None;
        }
        let entry = &self.person.log[self.stop_target()?];
        use pb::JobState as S;
        if entry.released() {
            let left = entry
                .entry
                .as_ref()
                .and_then(|e| e.left_running)
                .unwrap_or(0);
            let handle = entry.job.map(|j| worker::short(&j)).unwrap_or_default();
            return Some(if left > 0 {
                format!(
                    "shell exited · {} writing · /job cancel {handle} stops capture",
                    processes(left)
                )
            } else {
                "shell exited · output still open".into()
            });
        }
        if entry.stop.is_some() || matches!(entry.state(), Some(S::Stopping)) {
            return Some("stopping · Ctrl-C again kills".into());
        }
        if entry.state() == Some(S::CapturePaused) {
            return Some("paused: output quota full · Ctrl-C stops it".into());
        }
        let started = entry
            .entry
            .as_ref()
            .and_then(|e| e.intent_unix_ms)
            .filter(|at| *at > 0);
        let elapsed = started
            .map(|at| format!(" · {} s", now_unix_ms.saturating_sub(at) / 1000))
            .unwrap_or_default();
        Some(format!(
            "running {}{}{elapsed} · Ctrl-C stops it",
            if entry.outside {
                "OUTSIDE the sandbox "
            } else {
                ""
            },
            entry.excerpt()
        ))
    }
    /// Commands of the selected project that are queued or running, for the
    /// exit confirmation of a TUI-launched backend.
    pub(super) fn person_exit_line(&self) -> Option<String> {
        if !self.view.service.as_ref().is_some_and(|s| s.owned) {
            return None;
        }
        let count = self
            .queue_entries
            .iter()
            .filter(|e| command_entry(e) && matches!(e.state, Some(1..=3)))
            .count();
        match count {
            0 => None,
            1 => Some("1 command is queued or running; it stops when the service stops.".into()),
            n => Some(format!(
                "{n} commands are queued or running; they stop when the service stops."
            )),
        }
    }

    // Composer presentation.

    /// The hint row in command and outside mode.
    pub(super) fn person_hint(&self) -> Option<String> {
        if !self.mode.command() {
            return None;
        }
        let directory = self
            .person_directory()
            .map(|(_, display)| display)
            .unwrap_or_else(|| "?".into());
        let split = &self.person.split.1;
        // The counts lead the key help, so a narrow row keeps them.
        let mut hint = if self.mode == Mode::Outside {
            format!("↵ run OUTSIDE the sandbox in {directory}")
        } else {
            format!("↵ run in {directory}")
        };
        if !split.parsed && !self.editor.is_empty() {
            hint.push_str(" · not split");
        } else if split.commands() > 1 {
            hint.push_str(&format!(" · {} commands will run", split.commands()));
        }
        if split.hidden > 0 {
            hint.push_str(&format!(
                " · {} hidden character{}",
                split.hidden,
                if split.hidden == 1 { "" } else { "s" }
            ));
        }
        hint.push_str(if self.mode == Mode::Outside {
            " · ⌫ at start: sandboxed"
        } else {
            " · ⌫ at start: message"
        });
        Some(hint)
    }
    /// The split cached with the draft: recomputed only when the draft
    /// changed, so rendering performs no parsing.
    pub(super) fn person_refresh_split(&mut self) {
        if !self.mode.command() {
            return;
        }
        let text = self.editor.text();
        if self.person.split.0 != text {
            self.person.split = (text.clone(), split(&text));
        }
    }
    /// Rows the command view needs at `width` text cells.
    pub(super) fn person_rows(&self, width: u16) -> u16 {
        let rows = layout(&self.person.split.0, &self.person.split.1, width.into()).len();
        u16::try_from(rows).unwrap_or(u16::MAX)
    }
    /// Draws the command view: gutter marker or caret, rule rows, hidden
    /// marks and the selection, then places the cursor. `area` is the text
    /// area; the gutter is the two cells to its left.
    pub(super) fn draw_command_view(&mut self, frame: &mut Frame, area: Rect, focused: bool) {
        let mode = self.mode;
        let tint = mode.tint().unwrap_or(INPUT_TINT);
        let input = Style::default()
            .fg(MARKER_BLACK)
            .bg(tint)
            .add_modifier(Modifier::BOLD);
        let accent = Style::default().fg(mode.sent()).bg(tint);
        let mark = if mode == Mode::Outside {
            input
        } else {
            input.fg(HIDDEN_MARK)
        };
        let rows = layout(
            &self.person.split.0,
            &self.person.split.1,
            area.width.into(),
        );
        let cursor = self.editor.cursor_byte();
        let selection = self.editor.selection_bytes();
        let (cursor_row, cursor_x) = cursor_cell(&rows, cursor);
        let height = usize::from(area.height).max(1);
        let scroll = &mut self.person.scroll;
        if cursor_row < *scroll {
            *scroll = cursor_row;
        } else if cursor_row >= *scroll + height {
            *scroll = cursor_row + 1 - height;
        }
        // A view that grew, or a draft that shrank, shows as many rows as
        // fit: a scroll kept from an earlier draft never leaves rows blank.
        *scroll = (*scroll).min(rows.len().saturating_sub(height));
        let scroll = *scroll;
        let gutter_x = area.x.saturating_sub(2);
        for (offset, row) in rows.iter().skip(scroll).take(height).enumerate() {
            let y = area.y + offset as u16;
            let gutter = match row.gutter {
                Gutter::Marker => Some((mode.marker(), input)),
                Gutter::Caret => Some(("›", accent.add_modifier(Modifier::BOLD))),
                Gutter::Blank => None,
                Gutter::Rule => {
                    frame.render_widget(
                        Paragraph::new("─".repeat(usize::from(area.width) + 2)).style(accent),
                        Rect::new(gutter_x, y, area.width + 2, 1),
                    );
                    continue;
                }
            };
            if let Some((text, style)) = gutter {
                frame.render_widget(
                    Paragraph::new(text).style(style),
                    Rect::new(gutter_x, y, 2, 1),
                );
            }
            let spans: Vec<Span> = row
                .cells
                .iter()
                .map(|cell| {
                    let style = if cell.hidden { mark } else { input };
                    let style = if selection.contains(&cell.byte) {
                        style.add_modifier(Modifier::REVERSED)
                    } else {
                        style
                    };
                    Span::styled(cell.text.clone(), style)
                })
                .collect();
            frame.render_widget(
                Paragraph::new(Line::from(spans)).style(input),
                Rect::new(area.x, y, area.width, 1),
            );
        }
        if focused
            && cursor_row >= scroll
            && cursor_row < scroll + height
            && (cursor_x as u16) < area.width.max(1)
        {
            frame.set_cursor_position((
                area.x + cursor_x as u16,
                area.y + (cursor_row - scroll) as u16,
            ));
        }
    }
}

/// A staged command's submit facts, fixed when it is staged (PCR9), by its
/// local outbox row's request ID.
#[derive(Clone, Debug)]
pub(super) struct CommandRow {
    pub request_id: [u8; 16],
    pub mode: Mode,
    pub cwd: String,
    /// `!` only: the sanitized PATH and the PC3 client variables.
    pub environment: Option<(String, Vec<pb::PersonEnvironment>)>,
}

/// Person command state beside the App's other views.
#[derive(Default)]
pub(super) struct Person {
    /// Staged command rows' submit facts.
    pub rows: Vec<CommandRow>,
    /// The last observed secret store status (`/status`).
    pub secret_store: Option<pb::SecretStoreStatus>,
    pub launch: Launch,
    /// Whether the service lists capability 8 (OD3), for the current epoch.
    pub supported: Option<bool>,
    /// The draft the split was computed for, and the split.
    pub split: (String, Split),
    /// The command view's first shown row.
    pub scroll: usize,
    pub log: Vec<LogEntry>,
    /// The top row of each drawn log entry in the history, by log index.
    pub drawn: Vec<(usize, usize)>,
    /// A stop or kill waiting for the job request slot.
    pub deferred: Option<JobCommand>,
    /// `/paths` and the held entries' decisions (execution-path-guard.md).
    pub paths: super::path_guard::Paths,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::jobs::{Failure, Finished, Reply, Request as JobRequest};
    use crate::tui::queue::{Request as QueueRequest, Update as QueueUpdate};
    use crossterm::event::{Event, KeyEvent};
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};

    const PROJECT: [u8; 16] = [1; 16];
    fn launch() -> Launch {
        Launch {
            path: Some(b"/usr/bin::relative:/bin".to_vec()),
            environment: vec![
                (b"PATH".to_vec(), b"/usr/bin::relative:/bin".to_vec()),
                (b"LANG".to_vec(), b"en_US.UTF-8".to_vec()),
                (b"HOME".to_vec(), b"/Users/fixture".to_vec()),
                (b"TMPDIR".to_vec(), b"/tmp/fixture".to_vec()),
                (b"GITHUB_TOKEN".to_vec(), b"fixture".to_vec()),
            ],
        }
    }
    fn app() -> App {
        let mut app = App::new();
        app.view.connection = "connected";
        app.view.installation = "graph_ready";
        app.view.epoch = Some("epoch".into());
        app.projects = Some(vec![pb::ProjectReply {
            project_id: Some(PROJECT.to_vec()),
            location: Some("/projects/p".into()),
            name: Some("Fixture".into()),
            current: Some(true),
            ..Default::default()
        }]);
        app.selected_project = Some(PROJECT);
        app.launch_directory = Some("/projects/p/src/ui".into());
        app.restoration_ready = true;
        app.conversation_id = Some(vec![2; 16]);
        app.conversation_generation = 1;
        app.person.supported = Some(true);
        app.person.launch = launch();
        app
    }
    fn press(app: &mut App, code: KeyCode) -> bool {
        app.handle(Event::Key(KeyEvent::new(code, KeyModifiers::empty())))
    }
    fn chord(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> bool {
        app.handle(Event::Key(KeyEvent::new(code, modifiers)))
    }
    fn typed(app: &mut App, text: &str) {
        for c in text.chars() {
            press(app, KeyCode::Char(c));
        }
    }
    fn paste(app: &mut App, text: &str) {
        app.handle(Event::Paste(text.into()));
    }
    fn home(app: &mut App) {
        chord(app, KeyCode::Home, KeyModifiers::CONTROL);
    }
    fn render(app: &mut App, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        terminal.backend().buffer().clone()
    }
    fn text_of(buffer: &Buffer) -> String {
        buffer.content.iter().map(|c| c.symbol()).collect()
    }
    /// The composer row whose gutter shows `marker` on the box colour.
    fn marker_row(buffer: &Buffer, marker: &str, tint: Color) -> u16 {
        (0..buffer.area.height)
            .find(|y| {
                let cell = &buffer[(1, *y)];
                cell.symbol() == &marker[..1] && cell.bg == tint
            })
            .expect("marker row")
    }
    fn running(job: u8, outside: bool) -> LogEntry {
        LogEntry {
            project: Some(PROJECT),
            input: Some([job; 16]),
            job: Some([job; 16]),
            outside,
            command: Some("sleep 600".into()),
            entry: Some(pb::JobEntry {
                job_id: Some(vec![job; 16]),
                project_id: Some(PROJECT.to_vec()),
                origin: Some(2),
                outside_sandbox: outside.then_some(true),
                state: Some(pb::JobState::Running as i32),
                intent_unix_ms: Some(1_000),
                ..Default::default()
            }),
            ..Default::default()
        }
    }
    fn take_cancel(app: &mut App) -> Option<([u8; 16], bool)> {
        match app.take_job_request()?.command {
            JobCommand::Cancel { job, kill, .. } => Some((job, kill)),
            other => panic!("cancel expected, got {other:?}"),
        }
    }
    fn cancel_reply(app: &mut App, request: JobRequest, state: pb::JobState) {
        let JobCommand::Cancel {
            request_id, job, ..
        } = request.command.clone()
        else {
            panic!("cancel expected")
        };
        app.jobs_finished(Finished {
            request,
            result: Ok(Reply::Cancel(pb::JobCancelReply {
                request_id: Some(request_id.to_vec()),
                entry: Some(pb::JobEntry {
                    job_id: Some(job.to_vec()),
                    project_id: Some(PROJECT.to_vec()),
                    origin: Some(2),
                    state: Some(state as i32),
                    ..Default::default()
                }),
                requested: Some(true),
                error: None,
            })),
        });
    }
    fn staged_command(app: &mut App) -> pb::ConversationQueueCommandSubmit {
        match app.take_queue_request(std::time::Instant::now()) {
            Some(QueueRequest::Command(request)) => request,
            other => panic!("command submit expected, got {other:?}"),
        }
    }

    #[test]
    fn pc_u1_bang_on_an_empty_editor_enters_command_mode() {
        let mut app = app();
        press(&mut app, KeyCode::Char('!'));
        assert_eq!(app.mode, Mode::Command);
        assert!(app.editor.is_empty());
        let buffer = render(&mut app, 80, 24);
        let y = marker_row(&buffer, "!", COMMAND_BOX);
        assert_eq!(buffer[(1, y)].symbol(), "!");
    }
    #[test]
    fn pc_u2_bang_at_position_zero_keeps_the_text() {
        let mut app = app();
        typed(&mut app, "abc");
        home(&mut app);
        press(&mut app, KeyCode::Char('!'));
        assert_eq!(
            (app.mode, app.editor.text().as_str()),
            (Mode::Command, "abc")
        );
    }
    #[test]
    fn pc_u3_bang_elsewhere_is_text() {
        let mut app = app();
        typed(&mut app, "ab");
        press(&mut app, KeyCode::Left);
        press(&mut app, KeyCode::Char('!'));
        assert_eq!(
            (app.mode, app.editor.text().as_str()),
            (Mode::Message, "a!b")
        );
    }
    #[test]
    fn pc_u4_bang_with_the_command_list_open_is_text() {
        let mut app = app();
        typed(&mut app, "/");
        press(&mut app, KeyCode::Tab);
        assert!(matches!(app.overlay, Some(Overlay::Commands(..))));
        home(&mut app);
        press(&mut app, KeyCode::Char('!'));
        assert_eq!(app.mode, Mode::Message);
        assert!(app.editor.text().contains('!'));
    }
    #[test]
    fn pc_u5_and_u6_paste_enters_command_mode_only_at_position_zero() {
        let mut app = app();
        paste(&mut app, "!ls -l");
        assert_eq!(
            (app.mode, app.editor.text().as_str()),
            (Mode::Command, "ls -l")
        );
        let mut app = super::tests::app();
        typed(&mut app, "ls");
        press(&mut app, KeyCode::Left);
        paste(&mut app, "!x");
        assert_eq!(
            (app.mode, app.editor.text().as_str()),
            (Mode::Message, "l!xs")
        );
    }
    #[test]
    fn pc_u7_and_u8_backspace_at_start_leaves_or_deletes_the_selection() {
        let mut app = app();
        press(&mut app, KeyCode::Char('!'));
        typed(&mut app, "ls");
        home(&mut app);
        press(&mut app, KeyCode::Backspace);
        assert_eq!(
            (app.mode, app.editor.text().as_str()),
            (Mode::Message, "ls")
        );
        // A selection at position 0 is deleted first; the mode stays.
        let mut app = super::tests::app();
        press(&mut app, KeyCode::Char('!'));
        typed(&mut app, "ls");
        home(&mut app);
        chord(&mut app, KeyCode::Right, KeyModifiers::SHIFT);
        press(&mut app, KeyCode::Backspace);
        assert_eq!((app.mode, app.editor.text().as_str()), (Mode::Command, "s"));
    }
    #[test]
    fn pc_u9_and_u36_enter_with_blank_text_stages_nothing() {
        for outside in [false, true] {
            let mut app = app();
            press(&mut app, KeyCode::Char('!'));
            if outside {
                press(&mut app, KeyCode::Char('!'));
            }
            press(&mut app, KeyCode::Enter);
            typed(&mut app, "  ");
            press(&mut app, KeyCode::Enter);
            assert!(app.local_outbox.is_empty());
            assert_eq!(app.notice, TYPE_A_COMMAND);
            assert_eq!(
                app.mode,
                if outside {
                    Mode::Outside
                } else {
                    Mode::Command
                }
            );
            assert!(app.take_queue_request(std::time::Instant::now()).is_none());
        }
    }
    #[test]
    fn pc_u10_no_project_keeps_the_draft_and_mode() {
        let mut app = app();
        app.selected_project = None;
        press(&mut app, KeyCode::Char('!'));
        typed(&mut app, "ls");
        press(&mut app, KeyCode::Enter);
        assert!(app.local_outbox.is_empty());
        assert_eq!(app.notice, SELECT_PROJECT);
        assert_eq!(
            (app.mode, app.editor.text().as_str()),
            (Mode::Command, "ls")
        );
    }
    /// OD3: without capability 8, Enter keeps the draft with a notice.
    #[test]
    fn commands_need_capability_8() {
        let mut app = app();
        app.person.supported = None;
        press(&mut app, KeyCode::Char('!'));
        typed(&mut app, "ls");
        press(&mut app, KeyCode::Enter);
        assert!(app.local_outbox.is_empty());
        assert_eq!(app.notice, crate::tui::queue::COMMANDS_NOT_ENABLED);
        assert_eq!(
            (app.mode, app.editor.text().as_str()),
            (Mode::Command, "ls")
        );
    }
    #[test]
    fn pc_u11_enter_stages_a_command_row_and_returns_to_message_mode() {
        let mut app = app();
        press(&mut app, KeyCode::Char('!'));
        typed(&mut app, "ls");
        press(&mut app, KeyCode::Enter);
        assert!(app.editor.is_empty());
        assert_eq!(app.mode, Mode::Message);
        assert_eq!(app.local_outbox.len(), 1);
        assert_eq!(app.row_mode(&app.local_outbox[0]), Mode::Command);
        let request = staged_command(&mut app);
        assert_eq!(request.command.as_deref(), Some("ls"));
        assert_eq!(request.cwd.as_deref(), Some("src/ui"));
        // PC2: the sanitized client PATH; PC3: listed names only.
        assert_eq!(request.search_path.as_deref(), Some("/usr/bin:/bin"));
        assert_eq!(
            request
                .environment
                .iter()
                .map(|e| e.name.clone().unwrap())
                .collect::<Vec<_>>(),
            ["LANG"]
        );
        assert_eq!(request.outside_sandbox, None);
        assert!(request.launch_environment.is_empty());
        assert_eq!(request.conversation_id, Some(vec![2; 16]));
        assert_eq!(asura_control::person::environment_refusal(&request), None);
        assert!(asura_control::person::submit_shape(&request));
    }
    #[test]
    fn pc_u12_and_u40_recall_restores_each_entry_mode() {
        let mut app = app();
        app.history.record("hello");
        app.history.record(&recall_entry(Mode::Command, "ls"));
        app.history
            .record(&recall_entry(Mode::Outside, "brew upgrade"));
        app.history.record("!!ls");
        // The unsent draft keeps its mode.
        press(&mut app, KeyCode::Char('!'));
        typed(&mut app, "draft");
        let mut seen = Vec::new();
        for _ in 0..4 {
            press(&mut app, KeyCode::Up);
            seen.push((app.mode, app.editor.text()));
        }
        assert_eq!(
            seen,
            [
                (Mode::Message, "!!ls".to_owned()),
                (Mode::Outside, "brew upgrade".into()),
                (Mode::Command, "ls".into()),
                (Mode::Message, "hello".into()),
            ]
        );
        for _ in 0..3 {
            press(&mut app, KeyCode::Down);
        }
        assert_eq!(
            (app.mode, app.editor.text().as_str()),
            (Mode::Message, "!!ls")
        );
        press(&mut app, KeyCode::Down);
        assert_eq!(
            (app.mode, app.editor.text().as_str()),
            (Mode::Command, "draft")
        );
    }
    #[test]
    fn staged_commands_are_recorded_for_recall_with_their_mode() {
        let mut app = app();
        press(&mut app, KeyCode::Char('!'));
        press(&mut app, KeyCode::Char('!'));
        typed(&mut app, "make");
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Up);
        assert_eq!(
            (app.mode, app.editor.text().as_str()),
            (Mode::Outside, "make")
        );
    }
    #[test]
    fn pc_u13_help_entries_are_not_recorded() {
        let mut app = app();
        press(&mut app, KeyCode::Char('?'));
        assert!(app.help.is_some());
        typed(&mut app, "jobs");
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.history.previous(""), None);
        // In command mode `?` is text, not help.
        let mut app = super::tests::app();
        press(&mut app, KeyCode::Char('!'));
        press(&mut app, KeyCode::Char('?'));
        assert!(app.help.is_none());
        assert_eq!(app.editor.text(), "?");
    }
    #[test]
    fn pc_u14_command_mode_box_strips_marker_and_text_colours() {
        let mut app = app();
        press(&mut app, KeyCode::Char('!'));
        typed(&mut app, "ls");
        let buffer = render(&mut app, 80, 24);
        let y = marker_row(&buffer, "!", COMMAND_BOX);
        let marker = &buffer[(1, y)];
        assert_eq!((marker.fg, marker.bg), (MARKER_BLACK, COMMAND_BOX));
        assert!(marker.modifier.contains(Modifier::BOLD));
        let text = &buffer[(3, y)];
        assert_eq!(text.symbol(), "l");
        assert_eq!((text.fg, text.bg), (MARKER_BLACK, COMMAND_BOX));
        assert!(text.modifier.contains(Modifier::BOLD));
        // The whole box and both half-block strips.
        assert_eq!(buffer[(70, y)].bg, COMMAND_BOX);
        assert_eq!(buffer[(5, y - 1)].symbol(), "▄");
        assert_eq!(buffer[(5, y - 1)].fg, COMMAND_BOX);
        assert_eq!(buffer[(5, y + 1)].symbol(), "▀");
        assert_eq!(buffer[(5, y + 1)].fg, COMMAND_BOX);
        let hints = text_of(&buffer);
        assert!(hints.contains("↵ run in src/ui · ⌫ at start: message"));
    }
    fn page(stream: pb::JobStream, lines: std::ops::Range<u32>) -> pb::JobStreamPage {
        pb::JobStreamPage {
            stream: Some(stream as i32),
            start: Some(0),
            text: Some(
                lines
                    .map(|n| {
                        format!(
                            "{} {n}\n",
                            if stream == pb::JobStream::Stdout {
                                "out"
                            } else {
                                "err"
                            }
                        )
                    })
                    .collect(),
            ),
            state: Some(pb::JobPageState::Live as i32),
            ..Default::default()
        }
    }
    #[test]
    fn pc_u15_fold_shows_the_last_eight_lines_and_opens_the_viewer_at_the_end() {
        let mut entry = running(5, false);
        entry.pages = [
            Some(page(pb::JobStream::Stdout, 1..21)),
            Some(page(pb::JobStream::Stderr, 1..11)),
        ];
        let rows = entry_rows(&entry, Style::default());
        let texts: Vec<&str> = rows.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(texts[0], "! sleep 600");
        assert_eq!(rows[0].1.bg, Some(COMMAND_SENT));
        assert_eq!(rows[0].1.fg, Some(STRIPE_TEXT));
        assert_eq!(texts[1], "▸ 22 more lines");
        assert_eq!(texts[2], "stderr");
        assert_eq!(
            &texts[3..11],
            (3..11).map(|n| format!("err {n}")).collect::<Vec<_>>()
        );
        // Stdout first, then stderr under its label.
        entry.pages[1] = Some(page(pb::JobStream::Stderr, 1..3));
        let texts: Vec<String> = entry_rows(&entry, Style::default())
            .into_iter()
            .map(|(t, _)| t)
            .collect();
        assert_eq!(texts[1], "▸ 14 more lines");
        assert_eq!(
            &texts[2..10],
            [
                "out 15", "out 16", "out 17", "out 18", "out 19", "out 20", "stderr", "err 1"
            ]
        );
        // Enter and Space in log focus open the viewer at the end of stdout.
        for key in [KeyCode::Enter, KeyCode::Char(' ')] {
            let mut app = app();
            app.person.log.push(entry.clone());
            render(&mut app, 80, 30);
            // Shift+Up reaches a log that holds commands alone (no
            // transcript entry).
            assert!(app.transcript.is_empty());
            chord(&mut app, KeyCode::Up, KeyModifiers::SHIFT);
            assert!(app.history_focus, "Shift+Up enters a log of commands alone");
            press(&mut app, key);
            let request = app.take_job_request().expect("read");
            assert_eq!(
                request.command,
                JobCommand::Read {
                    job: [5; 16],
                    project: Some(PROJECT),
                    stream: worker::Stream::Stdout,
                    offset: None
                }
            );
            assert!(app.jobs.viewer.is_some());
        }
    }
    #[test]
    fn pc_u16_ctrl_c_stops_then_kills_and_never_quits() {
        let mut app = app();
        app.person.log.push(running(5, false));
        assert!(!chord(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL));
        let first = app.take_job_request().unwrap();
        assert!(
            matches!(first.command, JobCommand::Cancel { kill: false, job, .. } if job == [5; 16])
        );
        assert_eq!(app.hints(), "stopping · Ctrl-C again kills");
        cancel_reply(&mut app, first, pb::JobState::Stopping);
        assert!(!chord(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(take_cancel(&mut app), Some(([5; 16], true)));
        assert!(app.overlay.is_none());
        // Two presses before any reply: the kill waits for the request slot.
        let mut app = super::tests::app();
        app.person.log.push(running(6, false));
        chord(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        let first = app.take_job_request().unwrap();
        assert!(!chord(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.take_job_request().is_none());
        cancel_reply(&mut app, first, pb::JobState::Stopping);
        assert_eq!(take_cancel(&mut app), Some(([6; 16], true)));
    }
    #[test]
    fn pc_u17_ctrl_c_without_a_person_job_keeps_the_quit_rule() {
        let mut app = app();
        assert!(chord(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL));
        let mut app = super::tests::app();
        typed(&mut app, "draft");
        assert!(!chord(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(matches!(app.overlay, Some(Overlay::Exit(false))));
        // A settled person job is not a target.
        let mut app = super::tests::app();
        let mut settled = running(5, false);
        settled.entry.as_mut().unwrap().state = Some(pb::JobState::Settled as i32);
        app.person.log.push(settled);
        assert!(chord(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL));
    }
    #[test]
    fn pc_u18_cancel_twice_sends_one_stop_and_no_kill() {
        let mut app = app();
        app.person.log.push(running(5, false));
        typed(&mut app, "/cancel");
        press(&mut app, KeyCode::Enter);
        assert_eq!(take_cancel(&mut app), Some(([5; 16], false)));
        assert!(app.editor.is_empty());
        typed(&mut app, "/cancel");
        press(&mut app, KeyCode::Enter);
        assert!(app.take_job_request().is_none());
        assert!(app.person.deferred.is_none());
        assert_eq!(app.notice, "stopping · Ctrl-C again kills");
        // Command mode treats `/cancel` as shell text.
        let mut app = super::tests::app();
        app.person.log.push(running(6, false));
        press(&mut app, KeyCode::Char('!'));
        typed(&mut app, "/cancel");
        press(&mut app, KeyCode::Enter);
        assert!(app.take_job_request().is_none());
        assert_eq!(staged_command(&mut app).command.as_deref(), Some("/cancel"));
    }
    #[test]
    fn pc_u19_path_drops_empty_and_relative_entries_and_rejects_bad_forms() {
        assert_eq!(search_path(Some(b"/a::b:./c:/d")), Ok("/a:/d".into()));
        assert_eq!(
            search_path(Some(b"/a\0:/b")),
            Err(pb::QueueRefusal::PathInvalid)
        );
        let long = format!("/{}", "a".repeat(4096));
        assert_eq!(
            search_path(Some(long.as_bytes())),
            Err(pb::QueueRefusal::PathInvalid)
        );
        let many = vec!["/x"; 65].join(":");
        assert_eq!(
            search_path(Some(many.as_bytes())),
            Err(pb::QueueRefusal::PathInvalid)
        );
        let enough = vec!["/x"; 64].join(":");
        assert_eq!(search_path(Some(enough.as_bytes())), Ok(enough.clone()));
        assert_eq!(
            search_path(Some(b"::rel")),
            Err(pb::QueueRefusal::PathInvalid)
        );
        assert_eq!(search_path(None), Err(pb::QueueRefusal::PathInvalid));
        assert_eq!(
            search_path(Some(b"/a:\xff")),
            Err(pb::QueueRefusal::PathInvalid)
        );
        // A PATH without a usable entry refuses locally; draft and mode stay.
        let mut app = app();
        app.person.launch.path = Some(b"bin:".to_vec());
        press(&mut app, KeyCode::Char('!'));
        typed(&mut app, "ls");
        press(&mut app, KeyCode::Enter);
        assert!(app.local_outbox.is_empty());
        assert_eq!(app.notice, "PATH has no usable absolute entry.");
        assert_eq!(
            (app.mode, app.editor.text().as_str()),
            (Mode::Command, "ls")
        );
    }
    #[test]
    fn pc_u29_client_environment_sends_only_listed_names() {
        let request = |launch: Launch| {
            let mut app = app();
            app.person.launch = launch;
            press(&mut app, KeyCode::Char('!'));
            typed(&mut app, "ls");
            press(&mut app, KeyCode::Enter);
            staged_command(&mut app)
        };
        let mut fixture = launch();
        fixture.environment.extend([
            (b"USER".to_vec(), b"fixture".to_vec()),
            (b"ASURA_SECRET_1".to_vec(), b"x".to_vec()),
            (b"SSH_AUTH_SOCK".to_vec(), b"/tmp/agent.sock".to_vec()),
        ]);
        let sent = request(fixture.clone());
        let names: Vec<_> = sent
            .environment
            .iter()
            .map(|e| e.name.clone().unwrap())
            .collect();
        // Service-sourced names (HOME, TMPDIR, USER, ASURA_SECRET_n) and
        // unlisted names never come from the client.
        assert_eq!(names, ["LANG", "SSH_AUTH_SOCK"]);
        assert_eq!(asura_control::person::environment_refusal(&sent), None);
        for (name, value) in [
            ("LANG", "en US".to_owned()),
            ("SSH_AUTH_SOCK", "tmp/agent.sock".into()),
            ("SSH_AUTH_SOCK", format!("/{}", "s".repeat(103))),
        ] {
            let mut bad = fixture.clone();
            bad.environment
                .retain(|(n, _)| n.as_slice() != name.as_bytes());
            bad.environment
                .push((name.as_bytes().to_vec(), value.into_bytes()));
            assert_eq!(
                asura_control::person::environment_refusal(&request(bad)),
                Some(pb::QueueRefusal::EnvironmentInvalid),
                "{name}"
            );
        }
        let mut binary = fixture;
        binary.environment.retain(|(n, _)| n.as_slice() != b"LANG");
        binary.environment.push((b"LANG".to_vec(), vec![0xff]));
        assert_eq!(
            binary.client_variables(),
            Err(pb::QueueRefusal::EnvironmentInvalid)
        );
    }
    fn starts(text: &str) -> Vec<&str> {
        let split = split(text);
        assert!(split.parsed, "{text:?}");
        let mut bounds = split.starts.clone();
        bounds.push(text.len());
        bounds
            .windows(2)
            .map(|w| text[w[0]..w[1]].trim_end_matches([';', '&', '\n', ' ']))
            .collect()
    }
    #[test]
    fn pc_u30_splitter_table() {
        assert_eq!(starts("make; rm -rf build"), ["make", "rm -rf build"]);
        assert_eq!(starts("a && b || c | d"), ["a && b || c | d"]);
        assert_eq!(starts("sleep 1 & echo x"), ["sleep 1", "echo x"]);
        assert_eq!(starts("make 2>&1 &> log"), ["make 2>&1 &> log"]);
        assert_eq!(starts("a\nb"), ["a", "b"]);
        assert_eq!(starts("echo 'a;b' \"c;d\""), ["echo 'a;b' \"c;d\""]);
        assert_eq!(starts(r"echo a\;b"), [r"echo a\;b"]);
        assert_eq!(starts("cmd \\\n --flag"), ["cmd \\\n --flag"]);
        assert_eq!(starts("echo $(a; b)"), ["echo $(a; b)"]);
        assert_eq!(starts("echo `a; b`"), ["echo `a; b`"]);
        assert_eq!(starts("(cd x; make) ; ls"), ["(cd x; make)", "ls"]);
        assert_eq!(starts("echo \"a\nb\""), ["echo \"a\nb\""]);
        assert_eq!(starts("a;\n\n;b"), ["a", "b"]);
        assert_eq!(starts("a;"), ["a"]);
        for unsplit in [
            "echo 'open; b",
            "echo \"x; y",
            "echo $(a; b",
            "cat <<EOF\na; b\nEOF\n",
        ] {
            let split = split(unsplit);
            assert!(!split.parsed, "{unsplit:?}");
            assert_eq!(split.starts, [0]);
        }
        // Over the command bound the splitter does not run.
        assert!(!split(&"a;".repeat(4097)).parsed);
    }
    fn counts(text: &str) -> (usize, usize, usize) {
        let rows = layout(text, &split(text), 60);
        let markers = rows
            .iter()
            .filter(|r| matches!(r.gutter, Gutter::Marker | Gutter::Caret))
            .count();
        let rules = rows.iter().filter(|r| r.gutter == Gutter::Rule).count();
        (markers, rules, split(text).commands())
    }
    #[test]
    fn pc_u31_display_carets_rules_and_command_count() {
        assert_eq!(counts("make; rm -rf build"), (2, 1, 2));
        assert_eq!(counts("echo \"a\nb\""), (1, 0, 1));
        assert_eq!(counts("cmd \\\n --flag"), (1, 0, 1));
        assert_eq!(counts("one\ntwo\nthree"), (3, 2, 3));
        // Continuation rows show no caret.
        let rows = layout("cmd \\\n --flag", &split("cmd \\\n --flag"), 60);
        assert_eq!(
            rows.iter().map(|r| r.gutter.clone()).collect::<Vec<_>>(),
            [Gutter::Marker, Gutter::Blank]
        );
        let mut app = app();
        press(&mut app, KeyCode::Char('!'));
        paste(&mut app, "one\ntwo\nthree");
        let buffer = render(&mut app, 80, 24);
        let y = marker_row(&buffer, "!", COMMAND_BOX);
        let gutter: Vec<&str> = (y..y + 5).map(|r| buffer[(1, r)].symbol()).collect();
        assert_eq!(gutter, ["!", "─", "›", "─", "›"]);
        assert_eq!(buffer[(1, y + 1)].fg, COMMAND_SENT);
        assert!(text_of(&buffer).contains("3 commands will run"));
        // The cursor follows the end of the text in the last command row.
        app.mode = Mode::Command;
        assert_eq!(
            cursor_cell(&layout("ab;cd", &split("ab;cd"), 60), 5),
            (2, 2)
        );
        assert_eq!(
            cursor_cell(&layout("ab;cd", &split("ab;cd"), 60), 3),
            (2, 0)
        );
        assert!(app.hints().contains("3 commands will run"));
        let mut app = super::tests::app();
        press(&mut app, KeyCode::Char('!'));
        typed(&mut app, "echo 'open");
        render(&mut app, 80, 24);
        assert!(app.hints().contains(" · not split · "));
    }
    #[test]
    fn pc_u31_view_scroll_from_a_longer_draft_leaves_no_blank_rows() {
        // A long draft scrolls the view to its last rows; the next, shorter
        // draft shows from its first row (PC-E12 found the stale scroll).
        let mut app = app();
        press(&mut app, KeyCode::Char('!'));
        typed(&mut app, "a; b; c; d; e; f; g; h");
        render(&mut app, 80, 24);
        assert!(app.person.scroll > 0);
        press(&mut app, KeyCode::Enter);
        render(&mut app, 80, 24);
        paste(&mut app, "!one\ntwo\nthree");
        let buffer = render(&mut app, 80, 24);
        let y = marker_row(&buffer, "!", COMMAND_BOX);
        let gutter: Vec<&str> = (y..y + 5).map(|r| buffer[(1, r)].symbol()).collect();
        assert_eq!(gutter, ["!", "─", "›", "─", "›"]);
        assert_eq!(app.person.scroll, 0);
    }
    #[test]
    fn pc_u32_to_u35_outside_mode_steps() {
        // U32: `!` at 0 in command mode enters outside mode.
        let mut app = app();
        press(&mut app, KeyCode::Char('!'));
        typed(&mut app, "ls");
        home(&mut app);
        press(&mut app, KeyCode::Char('!'));
        assert_eq!(
            (app.mode, app.editor.text().as_str()),
            (Mode::Outside, "ls")
        );
        assert_eq!(app.mode.marker(), "!!");
        // U33: in outside mode `!` is text.
        press(&mut app, KeyCode::Char('!'));
        assert_eq!(
            (app.mode, app.editor.text().as_str()),
            (Mode::Outside, "!ls")
        );
        // U34: Backspace steps back one mode at a time.
        press(&mut app, KeyCode::Backspace);
        home(&mut app);
        press(&mut app, KeyCode::Backspace);
        assert_eq!(
            (app.mode, app.editor.text().as_str()),
            (Mode::Command, "ls")
        );
        press(&mut app, KeyCode::Backspace);
        assert_eq!(
            (app.mode, app.editor.text().as_str()),
            (Mode::Message, "ls")
        );
        // U35: pastes into an empty message box, and into command mode.
        for (text, mode, rest) in [
            ("!!x", Mode::Outside, "x"),
            ("!!!x", Mode::Outside, "!x"),
            ("! !x", Mode::Command, " !x"),
            ("!x", Mode::Command, "x"),
        ] {
            let mut app = super::tests::app();
            paste(&mut app, text);
            assert_eq!(
                (app.mode, app.editor.text().as_str()),
                (mode, rest),
                "{text}"
            );
        }
        let mut app = super::tests::app();
        press(&mut app, KeyCode::Char('!'));
        paste(&mut app, "!x");
        assert_eq!((app.mode, app.editor.text().as_str()), (Mode::Outside, "x"));
        // A selection at 0 in command mode: `!` replaces it as text.
        let mut app = super::tests::app();
        press(&mut app, KeyCode::Char('!'));
        typed(&mut app, "ls");
        home(&mut app);
        chord(&mut app, KeyCode::Right, KeyModifiers::SHIFT);
        press(&mut app, KeyCode::Char('!'));
        assert_eq!(
            (app.mode, app.editor.text().as_str()),
            (Mode::Command, "!s")
        );
    }
    #[test]
    fn pc_u37_message_text_starting_with_bangs_is_a_message() {
        let mut app = app();
        typed(&mut app, " !!ls");
        home(&mut app);
        press(&mut app, KeyCode::Delete);
        assert_eq!(
            (app.mode, app.editor.text().as_str()),
            (Mode::Message, "!!ls")
        );
        press(&mut app, KeyCode::Enter);
        assert!(matches!(
            app.take_queue_request(std::time::Instant::now()),
            Some(QueueRequest::Submit(request)) if request.prompt.as_deref() == Some("!!ls")
        ));
    }
    #[test]
    fn pc_u38_outside_mode_colours_hidden_mark_and_hint() {
        let mut app = app();
        press(&mut app, KeyCode::Char('!'));
        press(&mut app, KeyCode::Char('!'));
        typed(&mut app, "ls\u{202E}x");
        assert_eq!(app.editor.text(), "ls\u{202E}x");
        let buffer = render(&mut app, 100, 24);
        let y = marker_row(&buffer, "!!", OUTSIDE_BOX);
        for x in [1, 2] {
            let cell = &buffer[(x, y)];
            assert_eq!(
                (cell.symbol(), cell.fg, cell.bg),
                ("!", MARKER_BLACK, OUTSIDE_BOX)
            );
            assert!(cell.modifier.contains(Modifier::BOLD));
        }
        let mark = &buffer[(5, y)];
        assert_eq!(
            (mark.symbol(), mark.fg, mark.bg),
            ("█", MARKER_BLACK, OUTSIDE_BOX)
        );
        assert!(mark.modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(6, y)].symbol(), "x");
        assert_eq!(buffer[(5, y - 1)].fg, OUTSIDE_BOX);
        let all = text_of(&buffer);
        assert!(all.contains(
            "↵ run OUTSIDE the sandbox in src/ui · 1 hidden character · ⌫ at start: sandboxed"
        ));
        // In `!` mode the mark is hidden-mark red.
        let mut app = super::tests::app();
        press(&mut app, KeyCode::Char('!'));
        typed(&mut app, "a\u{200B}b");
        let buffer = render(&mut app, 80, 24);
        let y = marker_row(&buffer, "!", COMMAND_BOX);
        assert_eq!(
            (buffer[(4, y)].symbol(), buffer[(4, y)].fg),
            ("█", HIDDEN_MARK)
        );
    }
    #[test]
    fn pc_u39_outside_rows_show_stripe_warning_queue_and_jobs_marks() {
        let mut app = app();
        press(&mut app, KeyCode::Char('!'));
        press(&mut app, KeyCode::Char('!'));
        typed(&mut app, "brew upgrade");
        press(&mut app, KeyCode::Enter);
        // Staged: the warning is visible before the command can run.
        let staged = entry_rows(&app.person.log[0], Style::default());
        assert_eq!(staged[0].0, "!! brew upgrade");
        assert_eq!(staged[0].1.bg, Some(OUTSIDE_SENT));
        assert_eq!(staged[1].0, OUTSIDE_WARNING);
        assert_eq!(staged[1].1.fg, Some(ALERT));
        assert!(staged[1].1.add_modifier.contains(Modifier::BOLD));
        let buffer = render(&mut app, 80, 24);
        let all = text_of(&buffer);
        assert!(all.contains(OUTSIDE_WARNING));
        assert!(all.contains(">> Pending local · !! brew upgrade"));
        let request = staged_command(&mut app);
        assert_eq!(request.outside_sandbox, Some(true));
        assert!(request.search_path.is_none() && request.environment.is_empty());
        assert_eq!(request.launch_environment.len(), launch().environment.len());
        assert_eq!(asura_control::person::environment_refusal(&request), None);
        // Settled: the stripe and the warning stay.
        let mut settled = running(5, true);
        let job = settled.entry.as_mut().unwrap();
        job.state = Some(pb::JobState::Settled as i32);
        job.outcome = Some(1);
        job.exit_kind = Some(1);
        job.exit_value = Some(0);
        job.stdout_lines = Some(0);
        job.stderr_lines = Some(0);
        let rows = entry_rows(&settled, Style::default());
        assert_eq!(rows[0].0, "!! sleep 600");
        assert_eq!(rows[1].0, OUTSIDE_WARNING);
        // Queue panel and `/jobs` marks.
        let entry = pb::ConversationQueueEntry {
            kind: Some(rules::ENTRY_KIND),
            command_outside_sandbox: Some(true),
            text: Some("brew upgrade".into()),
            ..Default::default()
        };
        assert_eq!(queue_text(&entry), "!! brew upgrade");
        assert_eq!(worker::origin_mark(settled.entry.as_ref().unwrap()), "!!");
    }
    #[test]
    fn pc_u59_ctrl_c_targets_a_released_outside_job_only_without_a_model_turn() {
        let released = || {
            let mut entry = running(7, true);
            let job = entry.entry.as_mut().unwrap();
            job.state = Some(pb::JobState::Draining as i32);
            job.shell_exited = Some(true);
            job.left_running = Some(1);
            entry
        };
        let mut app = app();
        app.person.log.push(released());
        assert!(app.hints().starts_with(
            "shell exited · 1 background process writing · /job cancel 07070707 stops capture"
        ));
        assert!(!chord(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(take_cancel(&mut app), Some(([7; 16], false)));
        // A model turn is active: the existing turn rule (quit) applies.
        let mut app = super::tests::app();
        app.person.log.push(released());
        app.conversation_busy = true;
        app.conversation_cancellable = true;
        app.active_operation = Some(vec![3; 16]);
        assert!(chord(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.take_job_request().is_none());
        // The lane's running job is preferred over a released one.
        let mut app = super::tests::app();
        app.person.log.push(released());
        app.person.log.push(running(8, false));
        chord(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(take_cancel(&mut app), Some(([8; 16], false)));
    }
    #[test]
    fn status_row_shows_running_paused_and_outside_text() {
        let mut app = app();
        app.person.log.push(running(5, true));
        assert_eq!(
            app.person_status(43_500).unwrap(),
            "running OUTSIDE the sandbox sleep 600 · 42 s · Ctrl-C stops it"
        );
        app.person.log[0].outside = false;
        assert_eq!(
            app.person_status(43_500).unwrap(),
            "running sleep 600 · 42 s · Ctrl-C stops it"
        );
        app.person.log[0].entry.as_mut().unwrap().state = Some(pb::JobState::CapturePaused as i32);
        assert_eq!(
            app.person_status(0).unwrap(),
            "paused: output quota full · Ctrl-C stops it"
        );
    }
    #[test]
    fn footers_name_each_outcome() {
        let settled = |outcome, reason, exit: Option<(u32, u32)>, left| {
            let mut entry = running(5, left > 0);
            entry.directory = Some("src/ui".into());
            let job = entry.entry.as_mut().unwrap();
            job.state = Some(pb::JobState::Settled as i32);
            job.outcome = Some(outcome);
            job.reason = Some(reason);
            job.exit_kind = exit.map(|e| e.0);
            job.exit_value = exit.map(|e| e.1);
            job.stdout_lines = Some(3);
            job.stderr_lines = Some(1);
            job.elapsed_ms = Some(1_200);
            job.left_running = (left > 0).then_some(left);
            footer(&entry).unwrap()
        };
        assert_eq!(
            settled(1, 0, Some((1, 0)), 0),
            "↳ exit 0 · 4 lines · 1.2 s · src/ui"
        );
        assert_eq!(
            settled(3, 4, Some((2, 15)), 0),
            "↳ stopped by you (signal 15) · 4 lines · 1.2 s · src/ui"
        );
        assert_eq!(
            settled(3, 5, Some((2, 9)), 0),
            "↳ killed by you (signal 9) · 4 lines · 1.2 s · src/ui"
        );
        assert!(settled(3, 7, None, 0).starts_with("↳ stopped: service shutdown"));
        assert_eq!(
            settled(5, 11, None, 0),
            "↳ interrupted: service restarted; effects unknown"
        );
        assert_eq!(settled(4, 12, None, 0), "↳ not started: cwd_unavailable");
        assert_eq!(
            settled(4, 20, None, 0),
            "↳ not started: outside_sandbox_off"
        );
        assert_eq!(
            settled(1, 0, Some((1, 0)), 2),
            "↳ exit 0 · 4 lines · 1.2 s · src/ui · 2 background processes left running"
        );
        assert_eq!(
            settled(1, 4, Some((1, 0)), 2),
            "↳ exit 0 · output capture stopped by you · 4 lines · 1.2 s · src/ui · 2 background processes left running"
        );
        let mut released = running(5, true);
        let job = released.entry.as_mut().unwrap();
        job.state = Some(pb::JobState::Draining as i32);
        job.shell_exited = Some(true);
        job.exit_kind = Some(1);
        job.exit_value = Some(0);
        job.left_running = Some(2);
        assert_eq!(
            footer(&released).unwrap(),
            "↳ shell exited 0 · 2 background processes writing"
        );
        released.entry.as_mut().unwrap().left_running = None;
        assert_eq!(
            footer(&released).unwrap(),
            "↳ shell exited 0 · output still open"
        );
        // A draining entry without the shell's status (PC-E18 found the
        // double space).
        let job = released.entry.as_mut().unwrap();
        (job.exit_kind, job.exit_value, job.left_running) = (None, None, Some(1));
        assert_eq!(
            footer(&released).unwrap(),
            "↳ shell exited · 1 background process writing"
        );
    }
    #[test]
    fn refusals_return_the_command_to_the_editor_in_its_mode() {
        let mut app = app();
        press(&mut app, KeyCode::Char('!'));
        press(&mut app, KeyCode::Char('!'));
        typed(&mut app, "ls");
        press(&mut app, KeyCode::Enter);
        let request = staged_command(&mut app);
        app.queue_request_scope = app.queue_identity_scope();
        app.queue_finished(QueueUpdate {
            request: QueueRequest::Command(request.clone()),
            outcome: Ok(crate::tui::queue::refusal_reply(
                &request,
                pb::QueueRefusal::OutsideSandboxOff,
                None,
            )),
            done: true,
        });
        assert_eq!(
            app.notice,
            "Running outside the sandbox is turned off (personCommands.allowOutsideSandbox is false). Type it after a single ! to run it in the sandbox."
        );
        assert_eq!(
            (app.mode, app.editor.text().as_str()),
            (Mode::Outside, "ls")
        );
        assert!(app.local_outbox.is_empty() && app.person.log.is_empty());
    }
    #[test]
    fn acceptance_derives_the_job_id_from_the_input() {
        // The service stores the rewritten command: after the receipt the
        // log header shows the placeholder, not the typed token (PCR19).
        const TYPED: &str = "curl -H \"Authorization: Bearer abcdefghijklmnopqrstuvwx0123\" x";
        const STORED: &str = "curl -H \"Authorization: Bearer ${ASURA_SECRET_1}\" x";
        let mut app = app();
        press(&mut app, KeyCode::Char('!'));
        typed(&mut app, TYPED);
        press(&mut app, KeyCode::Enter);
        assert_eq!(header(&app.person.log[0]), format!("! {TYPED}"));
        let request = staged_command(&mut app);
        app.queue_request_scope = app.queue_identity_scope();
        let input = vec![9; 16];
        app.queue_finished(QueueUpdate {
            request: QueueRequest::Command(request.clone()),
            outcome: Ok(pb::ConversationQueueReply {
                request_id: request.request_id.clone(),
                accepted_input_id: Some(input.clone()),
                entries: vec![pb::ConversationQueueEntry {
                    input_id: Some(input.clone()),
                    project_id: Some(PROJECT.to_vec()),
                    conversation_id: Some(vec![2; 16]),
                    kind: Some(rules::ENTRY_KIND),
                    state: Some(1),
                    target_generation: Some(1),
                    command_cwd: Some("src/ui".into()),
                    command_outside_sandbox: Some(false),
                    text: Some(STORED.into()),
                    waiting_reason: Some(10),
                    waiting_text: Some("waiting: model turn running".into()),
                    sequence: Some(1),
                    ..Default::default()
                }],
                order_revision: Some(1),
                stale_order: Some(false),
                revision: Some(1),
                full_text: Some(false),
                ..Default::default()
            }),
            done: true,
        });
        assert!(app.local_outbox.is_empty());
        assert_eq!(app.notice, "Command waiting: model turn running");
        let entry = &app.person.log[0];
        assert_eq!(entry.input, Some([9; 16]));
        assert_eq!(entry.job, Some(job_of([9; 16])));
        assert_eq!(header(entry), format!("! {STORED}"));
        assert_eq!(
            app.person_folds().first().map(|t| t.job),
            Some(job_of([9; 16]))
        );
        let buffer = render(&mut app, 160, 24);
        let text = text_of(&buffer);
        assert!(text.contains(&format!(">> ! {STORED} · waiting: model turn running")));
        assert!(!text.contains("abcdefghij"), "the typed token is not shown");
    }
    #[test]
    fn refusal_texts_follow_the_design_table() {
        use pb::QueueRefusal as R;
        assert_eq!(
            refusal_text(R::EnvironmentInvalid, None, false),
            "This terminal's environment cannot be passed to a command (invalid or reserved variable name)."
        );
        assert_eq!(
            refusal_text(R::EnvironmentTooLarge, None, false),
            "This terminal's environment is too large to pass to a command (at most 1,024 variables and 40 KiB)."
        );
        assert_eq!(
            refusal_text(R::CwdInvalid, None, false),
            "The working directory cannot be used for commands."
        );
        assert_eq!(
            refusal_text(R::SecretStoreBusy, None, false),
            "The Keychain is busy. Press Enter again."
        );
        assert_eq!(
            refusal_text(R::SecretStoreLocked, None, false),
            "The login keychain is locked. Unlock it, then press Enter again."
        );
        assert_eq!(
            refusal_text(R::SecretStoreDenied, None, false),
            "macOS denied Asura access to its Keychain item."
        );
        assert!(refusal_text(R::SecretStoreUnavailable, Some(25_307), false).contains("-25307"));
        assert!(
            refusal_text(R::SecretStoreFull, None, true)
                .ends_with("/status lists Keychain items to delete.")
        );
        assert!(!refusal_text(R::SecretStoreFull, None, false).contains("/status"));
        for code in 1..=rules::MAX_REFUSAL {
            let refusal = R::try_from(code).unwrap();
            assert!(!refusal_text(refusal, Some(1), false).is_empty());
        }
    }
    #[test]
    fn status_lists_secret_store_notices() {
        let status = pb::SecretStoreStatus {
            item_count: Some(257),
            sweep_pending: Some(true),
            last_status: Some(25_307),
            stranded_count: Some(18),
            unrecognized_count: Some(2),
            stranded_inputs: (0..16u8).map(|n| vec![n; 16]).collect(),
        };
        let lines = secret_store_lines(Some(&status));
        assert_eq!(lines[0], "more than 256 Keychain items");
        assert_eq!(lines[1], "Keychain sweep pending");
        assert_eq!(lines[2], "Keychain unavailable: last OSStatus -25307");
        assert!(lines[3].starts_with("Asura cannot delete 18 old Keychain items. In Keychain Access, delete the login keychain items named 'Asura queued command secrets' with these accounts:"));
        assert_eq!(lines[4], format!("  input.{}", "00".repeat(16)));
        assert_eq!(lines[19], format!("  input.{}", "0f".repeat(16)));
        assert_eq!(lines[20], "  and 2 more");
        assert_eq!(
            lines[21],
            "2 Keychain items under Asura's service name were not written by Asura."
        );
        let quiet = pb::SecretStoreStatus {
            item_count: Some(1),
            last_status: Some(0),
            ..Default::default()
        };
        assert_eq!(secret_store_lines(Some(&quiet)), ["1 Keychain item"]);
        assert_eq!(
            secret_store_lines(Some(&pb::SecretStoreStatus::default())),
            ["Keychain items: not known until the first sweep"]
        );
        // `/status` opens the text panel from the last observation only.
        let mut app = app();
        app.set_person_service(None, Some(quiet));
        typed(&mut app, "/status");
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.result_title, "Service status");
        let text = app.config_result.clone().unwrap();
        assert!(text.starts_with("Asura · "));
        assert!(text.ends_with("1 Keychain item"));
        assert!(app.take_job_request().is_none());
        assert!(app.take_queue_request(std::time::Instant::now()).is_none());
    }
    #[test]
    fn restored_entries_follow_their_turns_and_fold_through_the_job_api() {
        let mut app = app();
        app.transcript = vec![("one".into(), "a".into()), ("two".into(), "b".into())];
        app.person_restored(vec![(0, [4; 16]), (2, [5; 16])]);
        assert_eq!(app.person.log.len(), 2);
        assert_eq!(app.person.log[0].after, 0);
        assert_eq!(app.person.log[1].job, Some(job_of([5; 16])));
        // The fold reads the job; its entry supplies the `!!` header.
        app.person_folded(FoldUpdate {
            job: job_of([5; 16]),
            result: Ok(pb::JobPage {
                entry: Some(pb::JobEntry {
                    job_id: Some(job_of([5; 16]).to_vec()),
                    project_id: Some(PROJECT.to_vec()),
                    origin: Some(2),
                    outside_sandbox: Some(true),
                    command: Some("brew upgrade".into()),
                    state: Some(pb::JobState::Settled as i32),
                    ..Default::default()
                }),
                pages: vec![page(pb::JobStream::Stdout, 1..3)],
                error: None,
            }),
            path_warnings: None,
        });
        let entry = &app.person.log[1];
        assert!(entry.fold_done);
        let rows = entry_rows(entry, Style::default());
        assert_eq!(rows[0].0, "!! brew upgrade");
        assert_eq!(rows[1].0, OUTSIDE_WARNING);
        // A job that does not exist yet waits for its queue entry to run.
        app.person_folded(FoldUpdate {
            job: job_of([4; 16]),
            result: Ok(pb::JobPage {
                error: Some(asura_control::jobs::error::NOT_FOUND.into()),
                ..Default::default()
            }),
            path_warnings: None,
        });
        assert!(app.person.log[0].fold_done);
        assert_eq!(app.person_folds().len(), 0);
        app.queue_entries = vec![pb::ConversationQueueEntry {
            input_id: Some(vec![4; 16]),
            conversation_id: Some(vec![2; 16]),
            kind: Some(rules::ENTRY_KIND),
            state: Some(3),
            ..Default::default()
        }];
        app.person_queue_seen();
        assert_eq!(app.person_folds().len(), 1);
        let buffer = render(&mut app, 80, 30);
        let all = text_of(&buffer);
        let first = all.find("waiting for its turn").or(all.find("! …"));
        let one = all.find("one").unwrap();
        assert!(first.is_some_and(|at| at < one));
    }
    #[test]
    fn fold_failures_are_shown_and_not_retried() {
        let mut app = app();
        app.person.log.push(running(5, false));
        app.person_folded(FoldUpdate {
            job: [5; 16],
            result: Err(Failure::Unavailable("service changed".into())),
            path_warnings: None,
        });
        assert!(app.person_folds().is_empty());
        assert!(
            entry_rows(&app.person.log[0], Style::default())
                .iter()
                .any(|(t, _)| t == "↳ output view unavailable: service changed")
        );
    }
    #[test]
    fn ctrl_j_and_ctrl_t_in_command_mode() {
        let mut app = app();
        press(&mut app, KeyCode::Char('!'));
        typed(&mut app, "a");
        chord(&mut app, KeyCode::Char('j'), KeyModifiers::CONTROL);
        typed(&mut app, "b");
        assert_eq!(app.editor.text(), "a\nb");
        chord(&mut app, KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert!(app.local_outbox.is_empty());
        chord(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert_eq!(staged_command(&mut app).command.as_deref(), Some("a\nb"));
    }
}
