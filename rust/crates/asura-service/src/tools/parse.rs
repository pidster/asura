//! The only parser of model tool-call arguments (tool-call-records.md, TCR-D5,
//! TCR-D11). It reads the received name and argument text into typed
//! [`Arguments`] or the lowest fault code that applies. It grants nothing;
//! the conversation owner decides, records and dispatches.
use super::{Arguments, MAX_PATH_BYTES, MAX_RESULT_BYTES};
use asura_platform::secrets::json::{self, Value};
use asura_storage::authority::conversation::{
    MAX_TOOL_ARGUMENTS, RegisteredTool, ToolFault as Fault, ToolOperation as Op,
};

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
                if command.contains('\0') {
                    m.fault(Fault::InvalidText);
                } else if command.is_empty() || command.len() > 8192 {
                    m.fault(Fault::OutOfRange);
                }
            }
            let cwd = m.string("cwd", false).unwrap_or(".");
            if !safe_cwd(cwd) {
                m.fault(Fault::UnsafePath);
            }
            // A missing `command` leaves the operation undetermined (TCR-D11).
            (
                command.map(|_| Op::ExecCmd),
                command.map(|command| Arguments::ExecCmd {
                    command: command.into(),
                    cwd: cwd.into(),
                }),
            )
        }
    }
}

#[cfg(test)]
#[path = "parse_tests.rs"]
mod tests;
