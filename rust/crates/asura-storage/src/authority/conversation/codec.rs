use super::*;
use crate::authority::{HEADER, MAX_FRAME, ReplayError, TRAILER};
use sha2::{Digest, Sha256};

type Result<T> = std::result::Result<T, ReplayError>;
struct Writer(Vec<u8>);
impl Writer {
    fn bytes(&mut self, b: &[u8]) {
        self.0.extend_from_slice(b);
    }
    fn byte(&mut self, n: u8) {
        self.0.push(n);
    }
    fn u64(&mut self, n: u64) {
        self.bytes(&n.to_be_bytes());
    }
    fn u32(&mut self, n: u32) {
        self.bytes(&n.to_be_bytes());
    }
    fn string(&mut self, s: &str, limit: usize) -> Result<()> {
        if s.len() > limit {
            return Err(ReplayError::Limit);
        }
        self.u32(s.len() as u32);
        self.bytes(s.as_bytes());
        Ok(())
    }
    fn optional(&mut self, id: Option<Id>) {
        self.byte(u8::from(id.is_some()));
        if let Some(id) = id {
            self.bytes(&id);
        }
    }
}
struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}
impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.offset.checked_add(n).ok_or(ReplayError::Corrupt)?;
        let bytes = self
            .bytes
            .get(self.offset..end)
            .ok_or(ReplayError::Corrupt)?;
        self.offset = end;
        Ok(bytes)
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self.take(N)?.try_into().unwrap())
    }
    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.array()?))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.array()?))
    }
    fn flag(&mut self) -> Result<bool> {
        match self.byte()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(ReplayError::Corrupt),
        }
    }
    fn optional(&mut self) -> Result<Option<Id>> {
        if self.flag()? {
            Ok(Some(self.array()?))
        } else {
            Ok(None)
        }
    }
    fn string(&mut self, limit: usize) -> Result<String> {
        let n = self.u32()? as usize;
        if n > limit {
            return Err(ReplayError::Limit);
        }
        Ok(std::str::from_utf8(self.take(n)?)
            .map_err(|_| ReplayError::Corrupt)?
            .to_owned())
    }
    fn cause(&mut self) -> Result<Cause> {
        match self.byte()? {
            0 => Ok(Cause::None),
            1 => Ok(Cause::UserCancel),
            2 => Ok(Cause::Deadline),
            3 => Ok(Cause::OutputLimit),
            4 => Ok(Cause::ServiceShutdown),
            5 => Ok(Cause::ProviderFailure),
            6 => Ok(Cause::ProtocolFailure),
            7 => Ok(Cause::Restart),
            8 => Ok(Cause::AuthorityFailure),
            9 => Ok(Cause::InputLimit),
            10 => Ok(Cause::Steering),
            _ => Err(ReplayError::Corrupt),
        }
    }
}

pub fn request_digest(request: Request<'_>) -> Result<Hash> {
    let mut w = Writer(Vec::new());
    match request {
        Request::Initialize { mode } => {
            if ![1, 2].contains(&mode) {
                return Err(ReplayError::Corrupt);
            }
            w.bytes(&1u16.to_be_bytes());
            w.byte(mode);
            w.u64(0);
        }
        Request::Register { location } => {
            valid_location(location)?;
            w.bytes(&4u16.to_be_bytes());
            w.string(location, 4096)?;
        }
        Request::RenameProject {
            project,
            expected_name_revision,
            name,
        } => {
            if project == [0; 16] {
                return Err(ReplayError::Corrupt);
            }
            capitalize_project_name(name)?;
            w.bytes(&20u16.to_be_bytes());
            w.bytes(&project);
            w.u64(expected_name_revision);
            w.string(name, 128)?;
        }
        Request::Submit {
            project,
            conversation,
            expected_generation,
            prompt,
        } => {
            if project == [0; 16]
                || prompt.is_empty()
                || (conversation.is_none() && expected_generation != 0)
                || conversation == Some([0; 16])
            {
                return Err(ReplayError::Corrupt);
            }
            w.bytes(&5u16.to_be_bytes());
            w.bytes(&project);
            w.optional(conversation);
            w.u64(expected_generation);
            w.string(prompt, MAX_PROMPT)?;
        }
        Request::Queue {
            project,
            conversation,
            target_operation,
            target_generation,
            kind,
            prompt,
        } => {
            if project == [0; 16]
                || conversation == [0; 16]
                || target_operation == [0; 16]
                || target_generation == 0
                || prompt.is_empty()
            {
                return Err(ReplayError::Corrupt);
            }
            w.bytes(&12u16.to_be_bytes());
            w.bytes(&project);
            w.bytes(&conversation);
            w.bytes(&target_operation);
            w.u64(target_generation);
            w.byte(kind as u8);
            w.string(prompt, MAX_PROMPT)?;
        }
        Request::QueueInput {
            project,
            conversation,
            expected_generation,
            new_conversation,
            prompt,
        } => {
            if project == [0; 16]
                || prompt.is_empty()
                || conversation == Some([0; 16])
                || (new_conversation && (conversation.is_some() || expected_generation != 0))
                || (!new_conversation && conversation.is_none())
            {
                return Err(ReplayError::Corrupt);
            }
            w.bytes(&18u16.to_be_bytes());
            w.bytes(&project);
            w.optional(conversation);
            w.u64(expected_generation);
            w.byte(u8::from(new_conversation));
            w.string(prompt, MAX_PROMPT)?;
        }
        Request::ReorderInput {
            input,
            after,
            expected_order_revision,
        } => {
            if input == [0; 16] || after == Some([0; 16]) || after == Some(input) {
                return Err(ReplayError::Corrupt);
            }
            w.bytes(&19u16.to_be_bytes());
            w.bytes(&input);
            w.optional(after);
            w.u64(expected_order_revision);
        }
        Request::InputDecision { input, action } => {
            if input == [0; 16] {
                return Err(ReplayError::Corrupt);
            }
            w.bytes(&13u16.to_be_bytes());
            w.bytes(&input);
            w.byte(action as u8);
        }
        Request::PromoteInput {
            input,
            target_operation,
            target_generation,
        } => {
            if input == [0; 16] || target_operation == [0; 16] || target_generation == 0 {
                return Err(ReplayError::Corrupt);
            }
            w.bytes(&16u16.to_be_bytes());
            w.bytes(&input);
            w.bytes(&target_operation);
            w.u64(target_generation);
        }
        Request::Cancel {
            operation,
            generation,
        } => {
            if operation == [0; 16] || generation == 0 {
                return Err(ReplayError::Corrupt);
            }
            w.bytes(&7u16.to_be_bytes());
            w.bytes(&operation);
            w.u64(generation);
        }
    }
    Ok(Sha256::digest(w.0).into())
}
pub(super) fn valid_location(s: &str) -> Result<()> {
    if s.len() > 4096 {
        return Err(ReplayError::Limit);
    }
    if !s.starts_with('/')
        || s.contains('\0')
        || (s.len() > 1
            && (s.ends_with('/')
                || s[1..]
                    .split('/')
                    .any(|c| c.is_empty() || c == "." || c == "..")))
    {
        return Err(ReplayError::Corrupt);
    }
    Ok(())
}
/// Canonical display capitalization. The original bytes remain in the rename
/// record so replay can independently verify this projection.
pub fn capitalize_project_name(name: &str) -> Result<String> {
    if name.is_empty() || name.len() > 128 || name.starts_with(' ') || name.ends_with(' ') {
        return Err(ReplayError::Corrupt);
    }
    let mut result = String::with_capacity(name.len());
    let mut found_alpha = false;
    let mut previous_space = false;
    for c in name.chars() {
        let bidi = matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}');
        if c.is_control() || bidi || (c.is_whitespace() && c != ' ') || (c == ' ' && previous_space)
        {
            return Err(ReplayError::Corrupt);
        }
        previous_space = c == ' ';
        if !found_alpha && c.is_alphabetic() {
            result.extend(c.to_uppercase());
            found_alpha = true;
        } else {
            result.push(c);
        }
    }
    if !found_alpha || result.len() > 128 {
        return Err(ReplayError::Corrupt);
    }
    Ok(result)
}
fn shell_parts(p: &ToolIntent) -> Result<(&str, &str)> {
    let (command, cwd) = p.path.split_once('\0').ok_or(ReplayError::Corrupt)?;
    if p.kind != SHELL_TOOL_KIND
        || command.is_empty()
        || command.len() > MAX_SHELL_COMMAND_BYTES
        || cwd.is_empty()
        || cwd.len() > MAX_SHELL_CWD_BYTES
        || cwd.contains('\0')
        || cwd.starts_with('/')
        || cwd.split('/').any(|part| part == "..")
        || !(1..=60_000).contains(&p.offset)
        || !(1..=60).contains(&p.limit)
        || p.offset > u64::from(p.limit) * 1000
    {
        return Err(ReplayError::Corrupt);
    }
    Ok((command, cwd))
}
impl ToolIntent {
    /// Stable identity for one admitted call, not permission to execute it again.
    pub fn shell_job_id(&self) -> Result<Id> {
        self.shell_arguments()?;
        let mut hash = Sha256::new();
        hash.update(b"asura.shell.job\0");
        hash.update(self.operation);
        hash.update(self.generation.to_be_bytes());
        hash.update(self.ordinal.to_be_bytes());
        let digest = hash.finalize();
        let mut id = [0; 16];
        id.copy_from_slice(&digest[..16]);
        if id == [0; 16] {
            id[15] = 1;
        }
        Ok(id)
    }
    /// Exact validated bytes; filesystem resolution remains the platform owner's task.
    pub fn shell_arguments(&self) -> Result<(&str, &str)> {
        valid_tool_intent(self)?;
        shell_parts(self)
    }
}
impl CreateNoteIdentity {
    pub fn derive(binding: [Id; 3], turn: Id, generation: u64, ordinal: u32) -> Result<Self> {
        if binding.contains(&[0; 16])
            || turn == [0; 16]
            || generation == 0
            || !(1..=8).contains(&ordinal)
        {
            return Err(ReplayError::Corrupt);
        }
        let derive = |domain: &[u8]| {
            let mut hash = Sha256::new();
            hash.update(domain);
            hash.update([0]);
            for id in binding {
                hash.update(id);
            }
            hash.update(turn);
            hash.update(generation.to_be_bytes());
            hash.update(ordinal.to_be_bytes());
            let bytes = hash.finalize();
            let mut id = [0; 16];
            id.copy_from_slice(&bytes[..16]);
            id[0] |= 0x80;
            id
        };
        Ok(Self {
            operation: derive(b"asura-memory-create-operation"),
            object: derive(b"asura-memory-create-object"),
            version: derive(b"asura-memory-create-version"),
            edge: derive(b"asura-memory-create-edge"),
        })
    }
}
impl MemoryCreateIntent {
    pub fn success_text(&self) -> String {
        let hex = |bytes: &[u8]| {
            bytes
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        };
        format!(
            "operation={}\nobject={}\nversion={}\nsha256={}\n",
            hex(&self.memory_operation),
            hex(&self.object),
            hex(&self.version),
            hex(&self.body_sha256)
        )
    }
    pub(crate) fn validate(&self) -> Result<()> {
        if self.operation == [0; 16]
            || self.generation == 0
            || !(1..=8).contains(&self.ordinal)
            || [
                self.project,
                self.memory_operation,
                self.object,
                self.version,
            ]
            .contains(&[0; 16])
            || !(1..=16_384).contains(&self.body_bytes)
            || self
                .source
                .is_some_and(|(source, edge)| source == [0; 16] || edge == [0; 16])
        {
            return Err(ReplayError::Corrupt);
        }
        Ok(())
    }
}

fn valid_tool_intent(p: &ToolIntent) -> Result<()> {
    if p.kind == 13 {
        return if p.operation != [0; 16]
            && p.generation > 0
            && (1..=8).contains(&p.ordinal)
            && p.path.is_empty()
            && p.offset == 0
            && p.limit == 0
        {
            Ok(())
        } else {
            Err(ReplayError::Corrupt)
        };
    }
    if p.kind == SHELL_TOOL_KIND {
        shell_parts(p)?;
        return if p.operation != [0; 16] && p.generation > 0 && (1..=8).contains(&p.ordinal) {
            Ok(())
        } else {
            Err(ReplayError::Corrupt)
        };
    }
    if matches!(p.kind, 6..=11) {
        let version = |value: &str| {
            value.len() == 32
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                && value.bytes().any(|byte| byte != b'0')
        };
        let arguments = match p.kind {
            6 => {
                (p.path.is_empty() || version(&p.path))
                    && p.offset == 0
                    && (1..=8).contains(&p.limit)
            }
            7 => {
                version(&p.path)
                    && (1..=16_384).contains(&p.limit)
                    && p.offset.checked_add(u64::from(p.limit)).is_some()
            }
            8 => version(&p.path) && p.offset == 0 && p.limit == 0,
            9 => p.path.len() <= 1024 && p.offset == 0,
            10 => p.path.len() <= 1024,
            11 => p.path.len() <= 1024 && p.offset == 0 && p.limit == 0,
            _ => false,
        };
        return if p.operation != [0; 16]
            && p.generation > 0
            && (1..=8).contains(&p.ordinal)
            && arguments
        {
            Ok(())
        } else {
            Err(ReplayError::Corrupt)
        };
    }
    if matches!(p.kind, 4 | 5) {
        return if p.operation != [0; 16]
            && p.generation > 0
            && (1..=8).contains(&p.ordinal)
            && !p.path.is_empty()
            && p.path.len() <= 1024
            && ((p.kind == 4 && (1..=16_384).contains(&p.limit))
                || (p.kind == 5 && p.offset == 0 && p.limit == 0))
        {
            Ok(())
        } else {
            Err(ReplayError::Corrupt)
        };
    }
    let path = (matches!(p.kind, 3 | 14 | 15) && p.path.is_empty())
        || !matches!(p.kind, 3 | 14 | 15)
            && !p.path.is_empty()
            && p.path.len() <= 1024
            && !p.path.chars().any(|c| c.is_control() || c == '\\')
            && ((p.kind == 2 && p.path == ".")
                || !p
                    .path
                    .split('/')
                    .any(|c| c.is_empty() || c == "." || c == ".."));
    let args = match p.kind {
        1 => {
            (1..=16_384).contains(&p.limit)
                && p.offset
                    .checked_add(u64::from(p.limit))
                    .is_some_and(|end| end <= i64::MAX as u64)
        }
        2 | 3 | 14 => p.offset == 0 && p.limit == 0,
        15 => p.offset == 0 && (1..=16).contains(&p.limit),
        _ => false,
    };
    if p.operation == [0; 16]
        || p.generation == 0
        || !(1..=8).contains(&p.ordinal)
        || !path
        || !args
    {
        return Err(ReplayError::Corrupt);
    }
    Ok(())
}
fn valid_tool_result(p: &ToolResult) -> Result<()> {
    if p.operation == [0; 16]
        || p.generation == 0
        || !(1..=8).contains(&p.ordinal)
        || !(1..=7).contains(&p.status)
        || p.text.len() > 16_384
        || (p.status != 1 && p.next_offset.is_some())
        || (!matches!(p.status, 1 | 5 | 6 | 7) && p.truncated)
    {
        return Err(ReplayError::Corrupt);
    }
    Ok(())
}

fn payload(record: &Record) -> Result<Vec<u8>> {
    let mut w = Writer(Vec::new());
    match record {
        Record::InputQueued(p) => {
            w.bytes(&p.request);
            w.bytes(&p.digest);
            w.bytes(&p.dispatch_request);
            w.bytes(&p.project);
            w.bytes(&p.conversation);
            w.bytes(&p.target_operation);
            w.u64(p.target_generation);
            w.optional(p.previous);
            w.byte(p.kind as u8);
            w.string(&p.prompt, MAX_PROMPT)?;
        }
        Record::InputQueuedV2(p) => {
            w.bytes(&p.request);
            w.bytes(&p.digest);
            w.bytes(&p.input);
            w.bytes(&p.dispatch_request);
            w.bytes(&p.project);
            w.bytes(&p.conversation);
            w.u64(p.expected_generation);
            w.byte(u8::from(p.new_conversation));
            w.string(&p.prompt, MAX_PROMPT)?;
            w.u64(p.order_revision);
        }
        Record::InputReordered(p) => {
            w.bytes(&p.request);
            w.bytes(&p.digest);
            w.bytes(&p.input);
            w.optional(p.after);
            w.u64(p.expected_order_revision);
            w.u64(p.order_revision);
        }
        Record::InputDecision(p) => {
            w.bytes(&p.request);
            w.bytes(&p.digest);
            w.bytes(&p.input);
            w.byte(p.action as u8);
        }
        Record::InputPromoted(p) => {
            w.bytes(&p.request);
            w.bytes(&p.digest);
            w.bytes(&p.input);
            w.bytes(&p.target_operation);
            w.u64(p.target_generation);
        }
        Record::PendingInit(p) => {
            w.bytes(&p.request);
            w.bytes(&p.digest);
            w.byte(p.mode);
            w.u64(p.configuration_revision);
            w.bytes(&p.configuration_digest);
            w.bytes(&p.graph);
        }
        Record::ActiveBinding(p) => {
            w.bytes(&p.request);
            w.u64(p.generation);
            w.bytes(&p.graph);
            w.bytes(&p.configuration_digest);
        }
        Record::OwnerGeneration => {}
        Record::ProjectRegistered(p) => {
            w.bytes(&p.request);
            w.bytes(&p.digest);
            w.bytes(&p.project);
            w.string(&p.location, 4096)?;
            w.u64(p.device);
            w.u64(p.inode);
            w.u64(p.registry_revision);
            w.byte(p.visibility);
        }
        Record::ProjectRequestAlias(p) => {
            w.bytes(&p.request);
            w.bytes(&p.digest);
            w.string(&p.location, 4096)?;
            w.bytes(&p.project);
            w.u64(p.registry_revision);
        }
        Record::ProjectRenamed(p) => {
            w.bytes(&p.request);
            w.bytes(&p.digest);
            w.bytes(&p.project);
            w.u64(p.expected_name_revision);
            w.u64(p.name_revision);
            w.string(&p.requested_name, 128)?;
            w.string(&p.name, 128)?;
        }
        Record::TurnAccepted(p) => {
            w.bytes(&p.request);
            w.bytes(&p.digest);
            w.bytes(&p.project);
            w.optional(p.original_conversation);
            w.u64(p.expected_generation);
            w.bytes(&p.conversation);
            w.u64(p.generation);
            w.bytes(&p.task);
            w.bytes(&p.operation);
            w.string(&p.model, 1024)?;
            w.bytes(&p.configuration_digest);
            w.bytes(&p.instructions_digest);
            w.bytes(&p.input_digest);
            if p.prior_operations.len() > 32 {
                return Err(ReplayError::Limit);
            }
            w.bytes(&(p.prior_operations.len() as u16).to_be_bytes());
            for id in &p.prior_operations {
                w.bytes(id);
            }
            w.string(&p.prompt, MAX_PROMPT)?;
            w.u32(p.reserved_output_tokens);
            w.u64(p.event_cursor);
        }
        Record::StartAuthorized(p) => {
            w.bytes(&p.operation);
            w.u64(p.generation);
            w.bytes(&p.helper);
            w.bytes(&p.input_digest);
        }
        Record::CancelRequested(p) => {
            w.bytes(&p.operation);
            w.u64(p.generation);
            w.byte(p.cause as u8);
        }
        Record::MemoryCreateIntent(p) => {
            p.validate()?;
            w.bytes(&p.operation);
            w.u64(p.generation);
            w.u32(p.ordinal);
            w.bytes(&p.project);
            w.bytes(&p.memory_operation);
            w.bytes(&p.object);
            w.bytes(&p.version);
            w.u32(p.body_bytes);
            w.bytes(&p.body_sha256);
            w.byte(u8::from(p.source.is_some()));
            if let Some((source, edge)) = p.source {
                w.bytes(&source);
                w.bytes(&edge);
            }
            w.bytes(&p.command_sha256);
        }
        Record::ToolIntent(p) => {
            valid_tool_intent(p)?;
            w.bytes(&p.operation);
            w.u64(p.generation);
            w.u32(p.ordinal);
            w.byte(p.kind);
            w.string(
                &p.path,
                if p.kind == SHELL_TOOL_KIND {
                    MAX_SHELL_INTENT_BYTES
                } else {
                    1024
                },
            )?;
            w.u64(p.offset);
            w.u32(p.limit);
        }
        Record::ToolResult(p) => {
            valid_tool_result(p)?;
            w.bytes(&p.operation);
            w.u64(p.generation);
            w.u32(p.ordinal);
            w.byte(p.status);
            w.string(&p.text, 16_384)?;
            w.byte(u8::from(p.next_offset.is_some()));
            if let Some(offset) = p.next_offset {
                w.u64(offset);
            }
            w.byte(u8::from(p.truncated));
        }
        Record::TurnTerminal(p) => {
            w.bytes(&p.operation);
            w.u64(p.generation);
            w.byte(p.kind as u8);
            w.byte(p.cause as u8);
            w.u64(p.final_cursor);
            w.byte(u8::from(p.usage_known));
            w.u32(p.output_tokens);
            w.u32(p.charged_tokens);
            w.string(&p.text, MAX_TEXT)?;
        }
    }
    Ok(w.0)
}
/// Encode a bounded frame. This checks representation, not state-dependent admission.
/// A future writer must validate the proposed transition against replay before I/O.
pub fn encode_frame(context: &FrameContext, record: &Record) -> Result<Vec<u8>> {
    if context.installation_id == [0; 16]
        || context.transition_id == [0; 16]
        || context.sequence == 0
        || context.owner_generation == 0
    {
        return Err(ReplayError::Corrupt);
    }
    let p = payload(record)?;
    let size = HEADER + p.len() + TRAILER;
    if size > MAX_FRAME {
        return Err(ReplayError::Limit);
    }
    let resulting = context
        .expected_revision
        .checked_add(1)
        .ok_or(ReplayError::Limit)?;
    let mut w = Writer(Vec::with_capacity(size));
    w.bytes(b"ASURAJ01");
    w.bytes(&1u16.to_be_bytes());
    w.bytes(&record.kind().to_be_bytes());
    w.u32(size as u32);
    w.u64(context.sequence);
    w.bytes(&context.prior_digest);
    w.bytes(&context.installation_id);
    w.bytes(&context.transition_id);
    let mut command = Sha256::new();
    command.update(record.kind().to_be_bytes());
    command.update(&p);
    w.bytes(&command.finalize());
    w.u64(context.expected_revision);
    w.u64(resulting);
    w.u64(context.owner_generation);
    w.bytes(&p);
    let digest: Hash = Sha256::digest(&w.0).into();
    w.bytes(&digest);
    w.bytes(b"ASURAC01");
    Ok(w.0)
}
/// Decode a payload from an envelope validated by the canonical frame parser.
pub fn decode_record(kind: u16, payload: &[u8]) -> Result<Record> {
    if payload.len() > MAX_FRAME - HEADER - TRAILER {
        return Err(ReplayError::Limit);
    }
    let mut r = Reader {
        bytes: payload,
        offset: 0,
    };
    let record = match kind {
        18 => Record::InputQueuedV2(InputQueuedV2 {
            request: r.array()?,
            digest: r.array()?,
            input: r.array()?,
            dispatch_request: r.array()?,
            project: r.array()?,
            conversation: r.array()?,
            expected_generation: r.u64()?,
            new_conversation: r.flag()?,
            prompt: r.string(MAX_PROMPT)?,
            order_revision: r.u64()?,
        }),
        19 => Record::InputReordered(InputReordered {
            request: r.array()?,
            digest: r.array()?,
            input: r.array()?,
            after: r.optional()?,
            expected_order_revision: r.u64()?,
            order_revision: r.u64()?,
        }),
        12 => Record::InputQueued(InputQueued {
            request: r.array()?,
            digest: r.array()?,
            dispatch_request: r.array()?,
            project: r.array()?,
            conversation: r.array()?,
            target_operation: r.array()?,
            target_generation: r.u64()?,
            previous: r.optional()?,
            kind: match r.byte()? {
                1 => InputKind::Queue,
                2 => InputKind::Steer,
                _ => return Err(ReplayError::Corrupt),
            },
            prompt: r.string(MAX_PROMPT)?,
        }),
        13 => Record::InputDecision(InputDecision {
            request: r.array()?,
            digest: r.array()?,
            input: r.array()?,
            action: match r.byte()? {
                1 => InputAction::Resume,
                2 => InputAction::Drop,
                3 => InputAction::Hold,
                _ => return Err(ReplayError::Corrupt),
            },
        }),
        16 => Record::InputPromoted(InputPromoted {
            request: r.array()?,
            digest: r.array()?,
            input: r.array()?,
            target_operation: r.array()?,
            target_generation: r.u64()?,
        }),
        10 => Record::PendingInit(PendingInit {
            request: r.array()?,
            digest: r.array()?,
            mode: r.byte()?,
            configuration_revision: r.u64()?,
            configuration_digest: r.array()?,
            graph: r.array()?,
        }),
        11 => Record::ActiveBinding(ActiveBinding {
            request: r.array()?,
            generation: r.u64()?,
            graph: r.array()?,
            configuration_digest: r.array()?,
        }),
        3 => Record::OwnerGeneration,
        4 => Record::ProjectRegistered(ProjectRegistered {
            request: r.array()?,
            digest: r.array()?,
            project: r.array()?,
            location: r.string(4096)?,
            device: r.u64()?,
            inode: r.u64()?,
            registry_revision: r.u64()?,
            visibility: r.byte()?,
        }),
        9 => Record::ProjectRequestAlias(ProjectRequestAlias {
            request: r.array()?,
            digest: r.array()?,
            location: r.string(4096)?,
            project: r.array()?,
            registry_revision: r.u64()?,
        }),
        20 => Record::ProjectRenamed(ProjectRenamed {
            request: r.array()?,
            digest: r.array()?,
            project: r.array()?,
            expected_name_revision: r.u64()?,
            name_revision: r.u64()?,
            requested_name: r.string(128)?,
            name: r.string(128)?,
        }),
        5 => {
            let request = r.array()?;
            let digest = r.array()?;
            let project = r.array()?;
            let original_conversation = r.optional()?;
            let expected_generation = r.u64()?;
            let conversation = r.array()?;
            let generation = r.u64()?;
            let task = r.array()?;
            let operation = r.array()?;
            let model = r.string(1024)?;
            let configuration_digest = r.array()?;
            let instructions_digest = r.array()?;
            let input_digest = r.array()?;
            let count = u16::from_be_bytes(r.array()?) as usize;
            if count > 32 {
                return Err(ReplayError::Limit);
            }
            let mut prior_operations = Vec::with_capacity(count);
            for _ in 0..count {
                prior_operations.push(r.array()?);
            }
            Record::TurnAccepted(Box::new(TurnAccepted {
                request,
                digest,
                project,
                original_conversation,
                expected_generation,
                conversation,
                generation,
                task,
                operation,
                model,
                configuration_digest,
                instructions_digest,
                input_digest,
                prior_operations,
                prompt: r.string(MAX_PROMPT)?,
                reserved_output_tokens: r.u32()?,
                event_cursor: r.u64()?,
            }))
        }
        6 => Record::StartAuthorized(StartAuthorized {
            operation: r.array()?,
            generation: r.u64()?,
            helper: r.array()?,
            input_digest: r.array()?,
        }),
        7 => Record::CancelRequested(CancelRequested {
            operation: r.array()?,
            generation: r.u64()?,
            cause: r.cause()?,
        }),
        8 => Record::TurnTerminal(TurnTerminal {
            operation: r.array()?,
            generation: r.u64()?,
            kind: match r.byte()? {
                1 => TerminalKind::Complete,
                2 => TerminalKind::Failed,
                3 => TerminalKind::Cancelled,
                4 => TerminalKind::Interrupted,
                _ => return Err(ReplayError::Corrupt),
            },
            cause: r.cause()?,
            final_cursor: r.u64()?,
            usage_known: r.flag()?,
            output_tokens: r.u32()?,
            charged_tokens: r.u32()?,
            text: r.string(MAX_TEXT)?,
        }),
        14 => {
            let operation = r.array()?;
            let generation = r.u64()?;
            let ordinal = r.u32()?;
            let kind = r.byte()?;
            let value = ToolIntent {
                operation,
                generation,
                ordinal,
                kind,
                path: r.string(if kind == SHELL_TOOL_KIND {
                    MAX_SHELL_INTENT_BYTES
                } else {
                    1024
                })?,
                offset: r.u64()?,
                limit: r.u32()?,
            };
            valid_tool_intent(&value)?;
            Record::ToolIntent(value)
        }
        15 => {
            let value = ToolResult {
                operation: r.array()?,
                generation: r.u64()?,
                ordinal: r.u32()?,
                status: r.byte()?,
                text: r.string(16_384)?,
                next_offset: if r.flag()? { Some(r.u64()?) } else { None },
                truncated: r.flag()?,
            };
            valid_tool_result(&value)?;
            Record::ToolResult(value)
        }
        17 => {
            let p = MemoryCreateIntent {
                operation: r.array()?,
                generation: r.u64()?,
                ordinal: r.u32()?,
                project: r.array()?,
                memory_operation: r.array()?,
                object: r.array()?,
                version: r.array()?,
                body_bytes: r.u32()?,
                body_sha256: r.array()?,
                source: if r.flag()? {
                    Some((r.array()?, r.array()?))
                } else {
                    None
                },
                command_sha256: r.array()?,
            };
            p.validate()?;
            Record::MemoryCreateIntent(p)
        }
        _ => return Err(ReplayError::UnsupportedFormat),
    };
    if r.offset != payload.len() {
        return Err(ReplayError::Corrupt);
    }
    Ok(record)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn project_names_capitalize_one_scalar_and_reject_unsafe_or_oversize_text() {
        assert_eq!(capitalize_project_name("my project").unwrap(), "My project");
        assert_eq!(
            capitalize_project_name("123ß model").unwrap(),
            "123SS model"
        );
        assert_eq!(capitalize_project_name("équipe").unwrap(), "Équipe");
        for invalid in [
            "",
            "123",
            " leading",
            "trailing ",
            "double  space",
            "line\nbreak",
            "a\tb",
            "a\u{202e}b",
            "a\u{2066}b",
            "a\u{00a0}b",
            "a".repeat(129).as_str(),
        ] {
            assert!(capitalize_project_name(invalid).is_err(), "{invalid:?}");
        }
        let expands_over_limit = format!("{}ǰ", "1".repeat(126));
        assert_eq!(expands_over_limit.len(), 128);
        assert!(capitalize_project_name(&expands_over_limit).is_err());
    }

    #[test]
    fn project_rename_record_has_kind_20_and_retains_exact_request_text() {
        let requested_name = "my project".to_owned();
        let record = Record::ProjectRenamed(ProjectRenamed {
            request: [1; 16],
            digest: request_digest(Request::RenameProject {
                project: [2; 16],
                expected_name_revision: 0,
                name: &requested_name,
            })
            .unwrap(),
            project: [2; 16],
            expected_name_revision: 0,
            name_revision: 1,
            requested_name,
            name: "My project".into(),
        });
        assert_eq!(record.kind(), 20);
        assert_eq!(
            decode_record(20, &payload(&record).unwrap()).unwrap(),
            record
        );
    }
    #[test]
    fn status_tool_intent_has_no_path_or_file_arguments() {
        let intent = ToolIntent {
            operation: [1; 16],
            generation: 1,
            ordinal: 1,
            kind: 3,
            path: String::new(),
            offset: 0,
            limit: 0,
        };
        assert!(valid_tool_intent(&intent).is_ok());
        let record = Record::ToolIntent(intent.clone());
        assert_eq!(
            decode_record(14, &payload(&record).unwrap()).unwrap(),
            record
        );
        for invalid in [
            ToolIntent {
                path: ".".into(),
                ..intent.clone()
            },
            ToolIntent {
                path: "file".into(),
                ..intent.clone()
            },
            ToolIntent {
                offset: 1,
                ..intent.clone()
            },
            ToolIntent {
                limit: 1,
                ..intent.clone()
            },
            ToolIntent {
                kind: 4,
                ..intent.clone()
            },
        ] {
            assert_eq!(valid_tool_intent(&invalid), Err(ReplayError::Corrupt));
        }
    }

    #[test]
    fn memory_tool_intents_use_exact_ids_limits_and_rejection_encodings() {
        let base = ToolIntent {
            operation: [1; 16],
            generation: 1,
            ordinal: 1,
            kind: 6,
            path: String::new(),
            offset: 0,
            limit: 8,
        };
        let valid_id = "01".repeat(16);
        for kind in 6..=11 {
            let intent = ToolIntent {
                kind,
                path: if kind == 6 {
                    String::new()
                } else if kind <= 8 {
                    valid_id.clone()
                } else {
                    "invalid\0\n".into()
                },
                offset: if kind == 10 { u64::MAX } else { 0 },
                limit: match kind {
                    6 => 8,
                    7 => 16_384,
                    9 | 10 => u32::MAX,
                    _ => 0,
                },
                ..base.clone()
            };
            let record = Record::ToolIntent(intent);
            assert_eq!(
                decode_record(14, &payload(&record).unwrap()).unwrap(),
                record
            );
        }
        for kind in [6, 7, 8] {
            for bad in [
                "0".repeat(32),
                "AA".repeat(16),
                "01".repeat(15),
                "01".repeat(17),
                "../note".into(),
            ] {
                let intent = ToolIntent {
                    kind,
                    path: bad,
                    limit: if kind == 8 { 0 } else { 1 },
                    ..base.clone()
                };
                assert_eq!(valid_tool_intent(&intent), Err(ReplayError::Corrupt));
            }
        }
        for limit in [0, 9, u32::MAX] {
            assert_eq!(
                valid_tool_intent(&ToolIntent {
                    limit,
                    ..base.clone()
                }),
                Err(ReplayError::Corrupt)
            );
        }
        for invalid in [
            ToolIntent {
                kind: 7,
                path: valid_id.clone(),
                offset: u64::MAX,
                limit: 1,
                ..base.clone()
            },
            ToolIntent {
                kind: 7,
                path: valid_id.clone(),
                limit: 16_385,
                ..base.clone()
            },
            ToolIntent {
                kind: 8,
                path: valid_id,
                offset: 1,
                limit: 0,
                ..base.clone()
            },
            ToolIntent {
                kind: 9,
                offset: 1,
                ..base.clone()
            },
            ToolIntent {
                kind: 10,
                path: "x".repeat(1025),
                ..base.clone()
            },
            ToolIntent {
                kind: 11,
                limit: 1,
                ..base.clone()
            },
        ] {
            assert_eq!(valid_tool_intent(&invalid), Err(ReplayError::Corrupt));
        }
    }
    #[test]
    fn audit_intent_is_bounded_and_cannot_select_a_log_path() {
        let intent = ToolIntent {
            operation: [1; 16],
            generation: 1,
            ordinal: 1,
            kind: 15,
            path: String::new(),
            offset: 0,
            limit: 16,
        };
        let record = Record::ToolIntent(intent.clone());
        assert_eq!(
            decode_record(14, &payload(&record).unwrap()).unwrap(),
            record
        );
        for invalid in [
            ToolIntent {
                path: "audit.jsonl".into(),
                ..intent.clone()
            },
            ToolIntent {
                offset: 1,
                ..intent.clone()
            },
            ToolIntent {
                limit: 0,
                ..intent.clone()
            },
            ToolIntent {
                limit: 17,
                ..intent
            },
        ] {
            assert_eq!(valid_tool_intent(&invalid), Err(ReplayError::Corrupt));
        }
    }
    #[test]
    fn inventory_intent_accepts_only_empty_arguments_and_preserves_encoding() {
        let intent = ToolIntent {
            operation: [1; 16],
            generation: 1,
            ordinal: 1,
            kind: 14,
            path: String::new(),
            offset: 0,
            limit: 0,
        };
        let record = Record::ToolIntent(intent.clone());
        assert_eq!(
            decode_record(14, &payload(&record).unwrap()).unwrap(),
            record
        );
        for invalid in [
            ToolIntent {
                path: "ignored".into(),
                ..intent.clone()
            },
            ToolIntent {
                offset: 1,
                ..intent.clone()
            },
            ToolIntent {
                limit: 1,
                ..intent.clone()
            },
            ToolIntent {
                kind: 12,
                ..intent.clone()
            },
            ToolIntent { kind: 17, ..intent },
        ] {
            assert_eq!(valid_tool_intent(&invalid), Err(ReplayError::Corrupt));
        }
    }
    #[test]
    fn initialize_digest_has_exact_domain_and_big_endian_fields() {
        let mut expected = vec![0, 1, 1];
        expected.extend_from_slice(&0u64.to_be_bytes());
        let expected: Hash = Sha256::digest(expected).into();
        assert_eq!(
            request_digest(Request::Initialize { mode: 1 }).unwrap(),
            expected
        );
        assert_ne!(
            expected,
            request_digest(Request::Initialize { mode: 2 }).unwrap()
        );
    }
    #[test]
    fn malformed_option_utf8_and_enum_are_rejected_before_indexing() {
        let terminal = Record::TurnTerminal(TurnTerminal {
            operation: [1; 16],
            generation: 1,
            kind: TerminalKind::Complete,
            cause: Cause::None,
            final_cursor: u64::MAX,
            usage_known: false,
            output_tokens: 0,
            charged_tokens: 512,
            text: "x".into(),
        });
        let mut bytes = payload(&terminal).unwrap();
        assert_eq!(decode_record(8, &bytes).unwrap(), terminal);
        bytes[24] = 255;
        assert_eq!(decode_record(8, &bytes).unwrap_err(), ReplayError::Corrupt);
        bytes = payload(&terminal).unwrap();
        bytes[34] = 2;
        assert_eq!(decode_record(8, &bytes).unwrap_err(), ReplayError::Corrupt);
        bytes = payload(&terminal).unwrap();
        *bytes.last_mut().unwrap() = 255;
        assert_eq!(decode_record(8, &bytes).unwrap_err(), ReplayError::Corrupt);
    }
    fn shell() -> ToolIntent {
        ToolIntent {
            operation: [1; 16],
            generation: 1,
            ordinal: 1,
            kind: SHELL_TOOL_KIND,
            path: "printf hello\0.".into(),
            offset: 1000,
            limit: 30,
        }
    }
    #[test]
    fn shell_exact_byte_bounds_and_kind_specific_encoding() {
        let base = shell();
        for cwd in [".", "./subdir", "a//b/", "a/./b"] {
            let value = ToolIntent {
                path: format!("echo hi\0{cwd}"),
                ..base.clone()
            };
            let rec = Record::ToolIntent(value.clone());
            assert_eq!(decode_record(14, &payload(&rec).unwrap()).unwrap(), rec);
            assert_eq!(value.shell_arguments().unwrap(), ("echo hi", cwd));
        }
        let max = ToolIntent {
            path: format!("{}\0{}", "x".repeat(8192), "y".repeat(1024)),
            offset: 60_000,
            limit: 60,
            ..base.clone()
        };
        let rec = Record::ToolIntent(max);
        let bytes = payload(&rec).unwrap();
        assert_eq!(decode_record(14, &bytes).unwrap(), rec);
        let mut old = bytes.clone();
        old[28] = 1;
        assert_eq!(decode_record(14, &old).unwrap_err(), ReplayError::Limit);
        for path in [
            "no delimiter".into(),
            "\0.".into(),
            "x\0".into(),
            "x\0a\0b".into(),
            "x\0/absolute".into(),
            "x\0../outside".into(),
            "x\0a/../b".into(),
            format!("{}\0.", "x".repeat(8193)),
            format!("x\0{}", "y".repeat(1025)),
            format!("{}\0.", "é".repeat(4097)),
        ] {
            assert_eq!(
                valid_tool_intent(&ToolIntent {
                    path,
                    ..base.clone()
                }),
                Err(ReplayError::Corrupt)
            );
        }
        for invalid in [
            ToolIntent {
                offset: 0,
                ..base.clone()
            },
            ToolIntent {
                offset: 60_001,
                limit: 60,
                ..base.clone()
            },
            ToolIntent {
                offset: 1001,
                limit: 1,
                ..base.clone()
            },
            ToolIntent {
                limit: 0,
                ..base.clone()
            },
            ToolIntent {
                limit: 61,
                ..base.clone()
            },
            ToolIntent {
                operation: [0; 16],
                ..base.clone()
            },
            ToolIntent {
                generation: 0,
                ..base.clone()
            },
            ToolIntent {
                ordinal: 9,
                ..base.clone()
            },
        ] {
            assert_eq!(invalid.shell_arguments(), Err(ReplayError::Corrupt));
        }
        // Existing record layout: operation16, generation8, ordinal4, kind1,
        // then the existing length-prefixed string and timeout integers.
        assert_eq!(bytes[28], 16);
        assert_eq!(
            &bytes[bytes.len() - 12..bytes.len() - 4],
            &60_000u64.to_be_bytes()
        );
        assert_eq!(&bytes[bytes.len() - 4..], &60u32.to_be_bytes());
    }
    #[test]
    fn shell_job_identity_has_fixed_domain_and_does_not_depend_on_arguments() {
        let base = shell();
        let expected = [
            0xde, 0x83, 0x9f, 0x8c, 0xd1, 0xb3, 0xc7, 0xa2, 0x16, 0x14, 0x9a, 0xaa, 0x29, 0x03,
            0x2f, 0xf6,
        ];
        assert_eq!(base.shell_job_id().unwrap(), expected);
        assert_eq!(
            ToolIntent {
                path: "different\0subdir".into(),
                ..base.clone()
            }
            .shell_job_id()
            .unwrap(),
            expected
        );
        for changed in [
            ToolIntent {
                operation: [2; 16],
                ..base.clone()
            },
            ToolIntent {
                generation: 2,
                ..base.clone()
            },
            ToolIntent { ordinal: 2, ..base },
        ] {
            assert_ne!(changed.shell_job_id().unwrap(), expected);
        }
    }
}
