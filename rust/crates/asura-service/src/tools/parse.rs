//! The only parser of model tool-call arguments (tool-call-records.md, TCR-D5,
//! TCR-D11). It reads the received name and argument text into typed
//! [`Arguments`] or the lowest fault code that applies. It grants nothing;
//! the conversation owner decides, records and dispatches.
use super::{Arguments, JobArguments, MAX_PATH_BYTES, MAX_RESULT_BYTES};
use asura_platform::secrets::json::{self, Value};
use asura_storage::authority::conversation::{
    Id, MAX_JOB_NOTE, MAX_JOB_PAGE, MAX_TOOL_ARGUMENTS, OutputStream, RegisteredTool,
    ToolFault as Fault, ToolOperation as Op,
};

/// exec_cmd `yield_seconds` (tool-job-controller.md, exec_cmd as a job).
pub const MAX_YIELD_SECONDS: u64 = 30;
pub const DEFAULT_YIELD_SECONDS: u64 = 10;
/// `job wait` `seconds` (the job tool).
pub const MAX_WAIT_SECONDS: u64 = 3_600;
/// `job read` `offset` upper bound, 2^63 - 1.
const MAX_JOB_OFFSET: u64 = i64::MAX as u64;
/// The exec_cmd `command` bound in UTF-8 bytes.
pub const MAX_COMMAND_BYTES: usize = 8_192;

/// The parser's reading of one call. `operation` is set when the name and the
/// text determine an operation, even if a later field is invalid.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Parsed {
    pub tool: Option<RegisteredTool>,
    pub operation: Option<Op>,
    pub arguments: Result<Arguments, Fault>,
}

/// The lowest fault code wins (TCR-D11, TCR-U04).
#[derive(Default)]
struct Faults(Option<Fault>);
impl Faults {
    fn add(&mut self, fault: Fault) {
        if self.0.is_none_or(|current| fault < current) {
            self.0 = Some(fault);
        }
    }
}

/// Reads `name` and `text` as received. `None` means the item was over its
/// size limit and only its size arrived.
pub fn parse_call(name: Option<&str>, text: Option<&str>) -> Parsed {
    let mut faults = Faults::default();
    let tool = name.and_then(RegisteredTool::lookup);
    match name {
        None => faults.add(Fault::NameOverLimit),
        Some(name) if name.chars().any(char::is_control) => faults.add(Fault::InvalidText),
        Some(_) if tool.is_none() => faults.add(Fault::UnregisteredName),
        Some(_) => {}
    }
    let value = match text {
        None => {
            faults.add(Fault::ArgumentsOverLimit);
            None
        }
        Some(text) if text.len() > MAX_TOOL_ARGUMENTS => {
            faults.add(Fault::ArgumentsOverLimit);
            None
        }
        Some(text) => match json::parse(text) {
            Ok(value) => Some(value),
            Err(json::ParseError::Invalid) => {
                faults.add(Fault::InvalidJson);
                None
            }
            Err(json::ParseError::Limit) => {
                faults.add(Fault::JsonLimit);
                None
            }
            Err(json::ParseError::DuplicateKey) => {
                faults.add(Fault::DuplicateKey);
                None
            }
        },
    };
    let mut operation = None;
    let mut arguments = None;
    match &value {
        Some(Value::Object(members)) => {
            if let Some(tool) = tool {
                let mut reader = Members::new(members, &mut faults);
                let (op, parsed) = read_tool(tool, &mut reader);
                reader.finish();
                operation = op;
                arguments = parsed;
            }
        }
        Some(_) => faults.add(Fault::NotObject),
        None => {}
    }
    Parsed {
        tool,
        operation,
        arguments: match (faults.0, arguments) {
            (None, Some(arguments)) => Ok(arguments),
            (Some(fault), _) => Err(fault),
            // Unreachable by construction: every path without a value adds a fault.
            (None, None) => Err(Fault::InvalidJson),
        },
    }
}

/// Object members. A member is consumed once; JSON `null` is an absent
/// optional member. Unconsumed members are `unexpected_field`.
struct Members<'a, 'f> {
    members: &'a [(String, Value)],
    used: Vec<bool>,
    faults: &'f mut Faults,
}
impl<'a, 'f> Members<'a, 'f> {
    fn new(members: &'a [(String, Value)], faults: &'f mut Faults) -> Self {
        Self {
            members,
            used: vec![false; members.len()],
            faults,
        }
    }
    fn take(&mut self, key: &str) -> Option<&'a Value> {
        let index = self.members.iter().position(|(k, _)| k == key)?;
        self.used[index] = true;
        match &self.members[index].1 {
            Value::Null => None,
            value => Some(value),
        }
    }
    fn string(&mut self, key: &str, required: bool) -> Option<&'a str> {
        match self.take(key) {
            None => {
                if required {
                    self.faults.add(Fault::MissingField);
                }
                None
            }
            Some(Value::String(text)) => Some(text),
            Some(_) => {
                self.faults.add(Fault::WrongType);
                None
            }
        }
    }
    /// A non-negative integer lexeme; a negative or too large value is out of
    /// range, a fraction, exponent or other type is the wrong type.
    fn integer(&mut self, key: &str) -> Option<u64> {
        match self.take(key)? {
            Value::Number(lexeme) => {
                let digits = lexeme.strip_prefix('-').unwrap_or(lexeme);
                if !digits.bytes().all(|b| b.is_ascii_digit()) {
                    self.faults.add(Fault::WrongType);
                    return None;
                }
                if lexeme.starts_with('-') && digits.bytes().any(|b| b != b'0') {
                    self.faults.add(Fault::OutOfRange);
                    return None;
                }
                match digits.parse::<u64>() {
                    Ok(value) => Some(value),
                    Err(_) => {
                        self.faults.add(Fault::OutOfRange);
                        None
                    }
                }
            }
            _ => {
                self.faults.add(Fault::WrongType);
                None
            }
        }
    }
    fn ranged(&mut self, key: &str, range: std::ops::RangeInclusive<u64>, default: u64) -> u64 {
        match self.integer(key) {
            Some(value) if range.contains(&value) => value,
            Some(_) => {
                self.faults.add(Fault::OutOfRange);
                default
            }
            None => default,
        }
    }
    fn version(&mut self, key: &str, required: bool) -> Option<String> {
        let value = self.string(key, required)?;
        if super::memory_id(value).is_err() {
            self.faults.add(Fault::InvalidVersionId);
        }
        Some(value.to_owned())
    }
    /// An optional integer within `range`; out of it is `out_of_range`.
    fn bounded(&mut self, key: &str, range: std::ops::RangeInclusive<u64>) -> Option<u64> {
        let value = self.integer(key)?;
        if range.contains(&value) {
            Some(value)
        } else {
            self.faults.add(Fault::OutOfRange);
            None
        }
    }
    /// The `job` handle (J3-E8): 32 lowercase hexadecimal digits, not all
    /// zero. A malformed string is `invalid_job_id`; another type is
    /// `wrong_type`.
    fn job_id(&mut self) -> Option<Id> {
        let text = self.string("job", true)?;
        match job_handle(text) {
            Some(id) => Some(id),
            None => {
                self.faults.add(Fault::InvalidJobId);
                None
            }
        }
    }
    fn fault(&mut self, fault: Fault) {
        self.faults.add(fault);
    }
    /// A member set to `null` is absent, so it is never unexpected; the SDK
    /// may write `null` for an optional schema member.
    fn finish(self) {
        if self
            .used
            .iter()
            .zip(self.members)
            .any(|(used, (_, value))| !used && *value != Value::Null)
        {
            self.faults.add(Fault::UnexpectedField);
        }
    }
}

/// A relative project path under the existing traversal rules.
fn safe_path(path: &str, allow_root: bool) -> bool {
    if allow_root && path == "." {
        return true;
    }
    !path.is_empty()
        && path.len() <= MAX_PATH_BYTES
        && !path.chars().any(|c| c.is_control() || c == '\\')
        && !path
            .split('/')
            .any(|c| c.is_empty() || c == "." || c == "..")
}
fn safe_cwd(cwd: &str) -> bool {
    !cwd.is_empty()
        && cwd.len() <= MAX_PATH_BYTES
        && !cwd.starts_with('/')
        && !cwd.contains('\0')
        && !cwd.split('/').any(|part| part == "..")
}

/// Reads a job handle (J3-E8): exactly 32 lowercase hexadecimal digits and
/// not all zero.
pub fn job_handle(text: &str) -> Option<Id> {
    let bytes = text.as_bytes();
    if bytes.len() != 32
        || !bytes
            .iter()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
    {
        return None;
    }
    let mut id = [0; 16];
    for (i, byte) in id.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).ok()?;
    }
    (id != [0; 16]).then_some(id)
}

/// The exec_cmd `command` text rule: no NUL or control character other
/// than tab and newline (fault 14).
fn command_text_valid(command: &str) -> bool {
    !command
        .chars()
        .any(|c| c.is_control() && c != '\t' && c != '\n')
}

fn command(reader: &mut Members<'_, '_>, commands: &[(&str, Op)]) -> Option<Op> {
    let name = reader.string("command", true)?;
    match commands.iter().find(|(text, _)| *text == name) {
        Some((_, op)) => Some(*op),
        None => {
            reader.fault(Fault::UnknownCommand);
            None
        }
    }
}

fn read_tool(tool: RegisteredTool, m: &mut Members<'_, '_>) -> (Option<Op>, Option<Arguments>) {
    match tool {
        RegisteredTool::Project => {
            let op = command(
                m,
                &[
                    ("read_file", Op::ReadFile),
                    ("list_directory", Op::ListDirectory),
                ],
            );
            let arguments = match op {
                Some(Op::ReadFile) => {
                    let path = m.string("path", true);
                    let offset = m.integer("offset").unwrap_or(0);
                    let limit = m.ranged("limit", 1..=MAX_RESULT_BYTES as u64, 4096);
                    if offset
                        .checked_add(limit)
                        .is_none_or(|end| end > i64::MAX as u64)
                    {
                        m.fault(Fault::OutOfRange);
                    }
                    path.map(|path| {
                        if !safe_path(path, false) {
                            m.fault(Fault::UnsafePath);
                        }
                        Arguments::ReadFile {
                            path: path.into(),
                            offset,
                            limit: limit as u32,
                        }
                    })
                }
                Some(_) => {
                    let path = m.string("path", false).unwrap_or(".");
                    if !safe_path(path, true) {
                        m.fault(Fault::UnsafePath);
                    }
                    Some(Arguments::ListDirectory { path: path.into() })
                }
                None => None,
            };
            (op, arguments)
        }
        RegisteredTool::Memory => {
            let op = command(
                m,
                &[
                    ("list_notes", Op::ListNotes),
                    ("get_note", Op::GetNote),
                    ("note_sources", Op::NoteSources),
                    ("create_note", Op::CreateNote),
                ],
            );
            let arguments = match op {
                Some(Op::ListNotes) => {
                    let after = m.version("after", false);
                    let limit = m.ranged("limit", 1..=8, 8) as u32;
                    Some(Arguments::MemoryListNotes { after, limit })
                }
                Some(Op::GetNote) => {
                    let version = m.version("version", true);
                    let offset = m.integer("offset").unwrap_or(0);
                    let limit = m.ranged("limit", 1..=MAX_RESULT_BYTES as u64, 4096);
                    if offset
                        .checked_add(limit)
                        .is_none_or(|end| end > i64::MAX as u64)
                    {
                        m.fault(Fault::OutOfRange);
                    }
                    version.map(|version| Arguments::MemoryGetNote {
                        version,
                        offset,
                        limit: limit as u32,
                    })
                }
                Some(Op::NoteSources) => m
                    .version("version", true)
                    .map(|version| Arguments::MemoryNoteSources { version }),
                Some(_) => {
                    let body = m.string("body", true);
                    if body.is_some_and(|body| body.is_empty() || body.len() > 16_384) {
                        m.fault(Fault::OutOfRange);
                    }
                    let source_version = m.version("source_version", false);
                    body.map(|body| Arguments::MemoryCreateNote {
                        body: body.into(),
                        source_version,
                    })
                }
                None => None,
            };
            (op, arguments)
        }
        RegisteredTool::Service => {
            let op = command(
                m,
                &[
                    ("status", Op::Status),
                    ("tools", Op::Tools),
                    ("audit", Op::Audit),
                ],
            );
            let arguments = match op {
                Some(Op::Status) => Some(Arguments::ObserveStatus),
                Some(Op::Tools) => Some(Arguments::ListTools),
                Some(_) => Some(Arguments::ReadAudit {
                    limit: m.ranged("limit", 1..=16, 16) as u32,
                }),
                None => None,
            };
            (op, arguments)
        }
        RegisteredTool::ExecCmd => {
            let command = m.string("command", true);
            if let Some(command) = command {
                if !command_text_valid(command) {
                    m.fault(Fault::InvalidText);
                } else if command.is_empty() || command.len() > MAX_COMMAND_BYTES {
                    m.fault(Fault::OutOfRange);
                }
            }
            let cwd = m.string("cwd", false).unwrap_or(".");
            if !safe_cwd(cwd) {
                m.fault(Fault::UnsafePath);
            }
            let yield_seconds = m.ranged(
                "yield_seconds",
                0..=MAX_YIELD_SECONDS,
                DEFAULT_YIELD_SECONDS,
            ) as u32;
            // A missing `command` leaves the operation undetermined (TCR-D11).
            (
                command.map(|_| Op::ExecCmd),
                command.map(|command| Arguments::ExecCmd {
                    command: command.into(),
                    cwd: cwd.into(),
                    yield_seconds,
                }),
            )
        }
        // The grouped job tool (tool-job-controller.md, the job tool). A
        // field the chosen command does not take stays unconsumed and is
        // `unexpected_field`.
        RegisteredTool::Job => {
            let op = command(
                m,
                &[
                    ("list", Op::JobList),
                    ("status", Op::JobStatus),
                    ("read", Op::JobRead),
                    ("wait", Op::JobWait),
                    ("cancel", Op::JobCancel),
                    ("note", Op::JobNote),
                ],
            );
            let arguments = match op {
                Some(Op::JobList) => Some(JobArguments::List {
                    before: m.bounded("before", 1..=u64::MAX),
                }),
                Some(Op::JobStatus) => m.job_id().map(|job| JobArguments::Status { job }),
                Some(Op::JobRead) => {
                    let job = m.job_id();
                    let stream = match m.string("stream", true) {
                        Some("stdout") => Some(OutputStream::Stdout),
                        Some("stderr") => Some(OutputStream::Stderr),
                        Some(_) => {
                            m.fault(Fault::OutOfRange);
                            None
                        }
                        None => None,
                    };
                    let offset = m.bounded("offset", 0..=MAX_JOB_OFFSET);
                    let limit = m.ranged("limit", 1..=u64::from(MAX_JOB_PAGE), 8_192) as u32;
                    job.zip(stream).map(|(job, stream)| JobArguments::Read {
                        job,
                        stream,
                        offset,
                        limit,
                    })
                }
                Some(Op::JobWait) => {
                    let job = m.job_id();
                    let seconds = m.bounded("seconds", 1..=MAX_WAIT_SECONDS).map(|s| s as u32);
                    job.map(|job| JobArguments::Wait { job, seconds })
                }
                Some(Op::JobCancel) => m.job_id().map(|job| JobArguments::Cancel { job }),
                Some(_) => {
                    let job = m.job_id();
                    let text = m.string("text", true);
                    if let Some(text) = text {
                        if text.contains('\0') {
                            m.fault(Fault::InvalidText);
                        } else if text.is_empty() || text.len() > MAX_JOB_NOTE {
                            m.fault(Fault::OutOfRange);
                        }
                    }
                    job.zip(text).map(|(job, text)| JobArguments::Note {
                        job,
                        text: text.into(),
                    })
                }
                None => None,
            };
            (op, arguments.map(Arguments::Job))
        }
    }
}

#[cfg(test)]
#[path = "parse_tests.rs"]
mod tests;
