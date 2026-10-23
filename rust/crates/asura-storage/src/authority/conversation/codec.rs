use super::*;
use crate::authority::{HEADER, MAX_FRAME, ReplayError, TRAILER};
use sha2::{Digest, Sha256};

type Result<T> = std::result::Result<T, ReplayError>;
pub(crate) struct Writer(pub(crate) Vec<u8>);
impl Writer {
    pub(crate) fn bytes(&mut self, b: &[u8]) {
        self.0.extend_from_slice(b);
    }
    pub(crate) fn byte(&mut self, n: u8) {
        self.0.push(n);
    }
    pub(crate) fn u64(&mut self, n: u64) {
        self.bytes(&n.to_be_bytes());
    }
    pub(crate) fn u32(&mut self, n: u32) {
        self.bytes(&n.to_be_bytes());
    }
    pub(crate) fn u16(&mut self, n: u16) {
        self.bytes(&n.to_be_bytes());
    }
    pub(crate) fn string(&mut self, s: &str, limit: usize) -> Result<()> {
        if s.len() > limit {
            return Err(ReplayError::Limit);
        }
        self.u32(s.len() as u32);
        self.bytes(s.as_bytes());
        Ok(())
    }
    pub(crate) fn optional(&mut self, id: Option<Id>) {
        self.byte(u8::from(id.is_some()));
        if let Some(id) = id {
            self.bytes(&id);
        }
    }
}
pub(crate) struct Reader<'a> {
    pub(crate) bytes: &'a [u8],
    pub(crate) offset: usize,
}
impl<'a> Reader<'a> {
    pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.offset.checked_add(n).ok_or(ReplayError::Corrupt)?;
        let bytes = self
            .bytes
            .get(self.offset..end)
            .ok_or(ReplayError::Corrupt)?;
        self.offset = end;
        Ok(bytes)
    }
    pub(crate) fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self.take(N)?.try_into().unwrap())
    }
    pub(crate) fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    pub(crate) fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.array()?))
    }
    pub(crate) fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.array()?))
    }
    pub(crate) fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.array()?))
    }
    /// A J1 string: a length over `limit` is `Corrupt`, like any other field.
    pub(crate) fn bounded_string(&mut self, limit: usize) -> Result<String> {
        let n = self.u32()? as usize;
        if n > limit {
            return Err(ReplayError::Corrupt);
        }
        Ok(std::str::from_utf8(self.take(n)?)
            .map_err(|_| ReplayError::Corrupt)?
            .to_owned())
    }
    pub(crate) fn flag(&mut self) -> Result<bool> {
        match self.byte()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(ReplayError::Corrupt),
        }
    }
    pub(crate) fn optional(&mut self) -> Result<Option<Id>> {
        if self.flag()? {
            Ok(Some(self.array()?))
        } else {
            Ok(None)
        }
    }
    pub(crate) fn string(&mut self, limit: usize) -> Result<String> {
        let n = self.u32()? as usize;
        if n > limit {
            return Err(ReplayError::Limit);
        }
        Ok(std::str::from_utf8(self.take(n)?)
            .map_err(|_| ReplayError::Corrupt)?
            .to_owned())
    }
    pub(crate) fn cause(&mut self) -> Result<Cause> {
        Cause::from_code(self.byte()?).ok_or(ReplayError::Corrupt)
    }
    pub(crate) fn finish(&self) -> Result<()> {
        require(self.offset == self.bytes.len())
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
/// The first 16 bytes of a SHA-256 digest; an all-zero result becomes 1 in
/// its final byte.
fn derived_id(hash: Sha256) -> Id {
    id_from_digest(&hash.finalize())
}
fn id_from_digest(digest: &[u8]) -> Id {
    let mut id = [0; 16];
    id.copy_from_slice(&digest[..16]);
    if id == [0; 16] {
        id[15] = 1;
    }
    id
}
/// The one job ID of every origin (J2-E6): the first 16 bytes of SHA-256
/// over ASCII `asura.job`, one zero byte, the origin byte, then the origin
/// reference. Model: the operation ID, the generation and the ordinal (big
/// endian). Person: the input ID. An all-zero result becomes 1 in its final
/// byte.
pub fn job_id(origin: &JobOrigin) -> Id {
    let mut hash = Sha256::new();
    hash.update(b"asura.job\0");
    hash.update([origin.code()]);
    match origin {
        JobOrigin::Model {
            operation,
            generation,
            ordinal,
        } => {
            hash.update(operation);
            hash.update(generation.to_be_bytes());
            hash.update(ordinal.to_be_bytes());
        }
        JobOrigin::Person { input } => hash.update(input),
    }
    derived_id(hash)
}
/// The job ID of one model exec_cmd call: [`job_id`] with the model origin.
/// Every exec_cmd call is a job (J3-E9).
pub fn exec_cmd_job_id(operation: Id, generation: u64, ordinal: u32) -> Id {
    job_id(&JobOrigin::Model {
        operation,
        generation,
        ordinal,
    })
}
/// The stdout and stderr payload IDs of a job: the single derivation
/// [`crate::payload::payload_id`] (job identity).
pub fn job_payload_ids(installation: Id, job: Id) -> [Id; 2] {
    crate::payload::job_payloads(installation, job)
}
impl CreateNoteIdentity {
    pub fn derive(binding: [Id; 3], turn: Id, generation: u64, ordinal: u32) -> Result<Self> {
        if binding.contains(&[0; 16])
            || turn == [0; 16]
            || generation == 0
            || !(1..=MAX_RECORDED_CALLS).contains(&ordinal)
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
impl CreateNoteCall {
    /// Derives the call from the installation binding (TCR-D23).
    pub fn derive(
        binding: [Id; 3],
        project: Id,
        operation: Id,
        generation: u64,
        ordinal: u32,
    ) -> Result<Self> {
        if project == [0; 16] {
            return Err(ReplayError::Corrupt);
        }
        Ok(Self {
            operation,
            generation,
            ordinal,
            project,
            identity: CreateNoteIdentity::derive(binding, operation, generation, ordinal)?,
        })
    }
    /// The text sent to the helper for a committed note (TCR-D1). Kind 15
    /// stores no text for create_note; this is rendered from derived IDs.
    pub fn success_text(&self) -> String {
        let hex = |bytes: &[u8]| {
            bytes
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        };
        format!(
            "operation={}\nobject={}\nversion={}\n",
            hex(&self.identity.operation),
            hex(&self.identity.object),
            hex(&self.identity.version),
        )
    }
}

fn valid_tool_result(p: &ToolResult) -> Result<()> {
    if p.operation == [0; 16]
        || p.generation == 0
        || !(1..=MAX_RECORDED_CALLS).contains(&p.ordinal)
        || !(1..=7).contains(&p.status)
        || p.text.len() > 16_384
        || (p.status != 1 && p.next_offset.is_some())
        || (!matches!(p.status, 1 | 5 | 6 | 7) && p.truncated)
    {
        return Err(ReplayError::Corrupt);
    }
    Ok(())
}

// J1 kinds 22 and 23. These tables are the stored codes. They are written out
// explicitly so that reordering a platform enum cannot change a stored code.
// A new secret kind needs a new rule-set revision and a new code.
/// Kind 22 field 8.
fn refusal_code(code: RefusalCode) -> u8 {
    match code {
        RefusalCode::PlaceholderNotAvailable => 1,
        RefusalCode::UnparsedCommand => 2,
        RefusalCode::SecretInHeredoc => 3,
        RefusalCode::SecretInSubstitution => 4,
        RefusalCode::SecretInInnerScript => 5,
        RefusalCode::SecretSingleQuoted => 6,
        RefusalCode::SecretAnsiQuoted => 7,
        RefusalCode::SecretMixedQuoting => 8,
        RefusalCode::SecretWithShellSyntax => 9,
        RefusalCode::TooManySecrets => 10,
        RefusalCode::ResidualSecret => 11,
        RefusalCode::DetectionUnavailable => 12,
        RefusalCode::RewrittenCommandTooLong => 13,
        RefusalCode::SecretInCwd => 14,
    }
}
fn refusal_from(code: u8) -> Result<RefusalCode> {
    Ok(match code {
        1 => RefusalCode::PlaceholderNotAvailable,
        2 => RefusalCode::UnparsedCommand,
        3 => RefusalCode::SecretInHeredoc,
        4 => RefusalCode::SecretInSubstitution,
        5 => RefusalCode::SecretInInnerScript,
        6 => RefusalCode::SecretSingleQuoted,
        7 => RefusalCode::SecretAnsiQuoted,
        8 => RefusalCode::SecretMixedQuoting,
        9 => RefusalCode::SecretWithShellSyntax,
        10 => RefusalCode::TooManySecrets,
        11 => RefusalCode::ResidualSecret,
        12 => RefusalCode::DetectionUnavailable,
        13 => RefusalCode::RewrittenCommandTooLong,
        14 => RefusalCode::SecretInCwd,
        _ => return Err(ReplayError::Corrupt),
    })
}
/// Secret kind codes 1–33: the positions of `SecretKind::ALL` at rule-set revision 1.
fn kind_code(kind: SecretKind) -> u8 {
    use SecretKind::*;
    match kind {
        PrivateKey => 1,
        AwsAccessKey => 2,
        AwsSecretKey => 3,
        GithubToken => 4,
        GitlabToken => 5,
        PypiToken => 6,
        DockerToken => 7,
        NpmToken => 8,
        BitbucketToken => 9,
        VaultToken => 10,
        FlyToken => 11,
        WebhookSecret => 12,
        MailgunKey => 13,
        GoogleOauth => 14,
        AzureKey => 15,
        SlackToken => 16,
        SlackWebhook => 17,
        ChatWebhook => 18,
        StripeKey => 19,
        AnthropicKey => 20,
        OpenaiKey => 21,
        GoogleApiKey => 22,
        Jwt => 23,
        UrlPassword => 24,
        UrlSecret => 25,
        AuthHeader => 26,
        SessionCookie => 27,
        PasswordHash => 28,
        AssignedSecret => 29,
        EnvSecret => 30,
        CliPassword => 31,
        NetrcPassword => 32,
        WifiPassword => 33,
    }
}
fn kind_from(code: u8) -> Result<SecretKind> {
    use SecretKind::*;
    Ok(match code {
        1 => PrivateKey,
        2 => AwsAccessKey,
        3 => AwsSecretKey,
        4 => GithubToken,
        5 => GitlabToken,
        6 => PypiToken,
        7 => DockerToken,
        8 => NpmToken,
        9 => BitbucketToken,
        10 => VaultToken,
        11 => FlyToken,
        12 => WebhookSecret,
        13 => MailgunKey,
        14 => GoogleOauth,
        15 => AzureKey,
        16 => SlackToken,
        17 => SlackWebhook,
        18 => ChatWebhook,
        19 => StripeKey,
        20 => AnthropicKey,
        21 => OpenaiKey,
        22 => GoogleApiKey,
        23 => Jwt,
        24 => UrlPassword,
        25 => UrlSecret,
        26 => AuthHeader,
        27 => SessionCookie,
        28 => PasswordHash,
        29 => AssignedSecret,
        30 => EnvSecret,
        31 => CliPassword,
        32 => NetrcPassword,
        33 => WifiPassword,
        _ => return Err(ReplayError::Corrupt),
    })
}
fn loss_code(reason: LossReason) -> u8 {
    match reason {
        LossReason::LineOverLimit => 1,
        LossReason::KeyBlockOverLimit => 2,
        LossReason::DetectorError => 3,
    }
}
fn loss_from(code: u8) -> Result<LossReason> {
    Ok(match code {
        1 => LossReason::LineOverLimit,
        2 => LossReason::KeyBlockOverLimit,
        3 => LossReason::DetectorError,
        _ => return Err(ReplayError::Corrupt),
    })
}
fn origin_code(origin: NumberOrigin) -> u8 {
    match origin {
        NumberOrigin::Command => 1,
        NumberOrigin::Output => 2,
    }
}
fn origin_from(code: u8) -> Result<NumberOrigin> {
    Ok(match code {
        1 => NumberOrigin::Command,
        2 => NumberOrigin::Output,
        _ => return Err(ReplayError::Corrupt),
    })
}
/// Kind 14 exec_cmd secret variables, refusal word, kind 23 numbers and spans.
const MAX_EXEC_CMD_VARIABLES: usize = 16;
const MAX_REFUSAL_WORD: u16 = 4096;
const MAX_CAPTURE_SPANS: u64 = 64;
const MAX_CAPTURE_NUMBER: u16 = 16_384;
const MAX_CAPTURE_NUMBERS: usize = 256;
const MIB: u64 = 1024 * 1024;
/// Kind 14: 98 bytes of fixed fields, a 512-byte name, 256 number kinds and
/// 64,000 bytes of arguments (tool-call-records.md, TCR-U03).
pub(crate) const MAX_TOOL_CALL_PAYLOAD: usize = 64_866;

fn source_code(source: ToolCallSource) -> u8 {
    source as u8
}
fn source_from(code: u8) -> Result<ToolCallSource> {
    Ok(match code {
        1 => ToolCallSource::GeneratedContent,
        2 => ToolCallSource::ProviderJson,
        _ => return Err(ReplayError::Corrupt),
    })
}

fn form_from(form: u8, reason: u8) -> Result<ArgumentForm> {
    Ok(match (form, reason) {
        (1, 0) => ArgumentForm::Verbatim,
        (2, 0) => ArgumentForm::Canonical,
        (3, 0) => ArgumentForm::Unparsed,
        (4, 1) => ArgumentForm::NotRetained(NotRetainedReason::Oversized),
        (4, 2) => ArgumentForm::NotRetained(NotRetainedReason::DetectionUnavailable),
        (4, 3) => ArgumentForm::NotRetained(NotRetainedReason::UnverifiableEscape),
        _ => return Err(ReplayError::Corrupt),
    })
}
impl ToolCall {
    /// Every kind 14 field rule that the record alone determines, including
    /// the derived registry lookup and the placeholder counts (TCR-D4,
    /// TCR-D20). Replay adds the operation, ordinal and cancel rules.
    pub(crate) fn validate(&self) -> Result<()> {
        use NotRetainedReason as R;
        use ToolDecision as D;
        use ToolFault as F;
        let decision = self.decision;
        let reason = self.form.not_retained();
        let named = !self.name.is_empty();
        let tool = RegisteredTool::lookup(&self.name);
        let exec = self.tool_operation == Some(ToolOperation::ExecCmd);
        let rules = self.rule_set_revision != 0;
        let fault = self.fault;
        let received_text = self.arguments_bytes as usize <= MAX_TOOL_ARGUMENTS;
        let shape = self.operation != [0; 16]
            && self.generation >= 1
            && (1..=MAX_TOOL_CALL_ORDINAL).contains(&self.ordinal)
            // The last ordinal is the over-limit record; after cancel or turn end it
            // is `cancelled` like any other call (TCR-D30).
            && (self.ordinal != MAX_TOOL_CALL_ORDINAL
                || matches!(decision, D::Limit | D::Cancelled))
            && self.name.len() <= MAX_STORED_TOOL_NAME
            && self.name_bytes >= 1
            && (!named || self.name_bytes as usize <= MAX_TOOL_NAME)
            && match self.form {
                ArgumentForm::NotRetained(_) => self.arguments.is_empty(),
                _ => self.arguments.len() <= MAX_TOOL_ARGUMENTS,
            }
            && (received_text || reason == Some(R::Oversized))
            && self.number_kinds.len() <= MAX_TOOL_NUMBERS;
        // Operation: derived from a retained, registered name and parsed text.
        let operation = match self.tool_operation {
            None => true,
            Some(op) => {
                tool == Some(op.tool())
                    && received_text
                    && !matches!(
                        self.form,
                        ArgumentForm::Unparsed | ArgumentForm::NotRetained(R::UnverifiableEscape)
                    )
            }
        };
        let faults = match decision {
            D::InvalidArguments => fault.is_some_and(|f| f != F::UnregisteredName),
            D::UnknownTool => fault == Some(F::UnregisteredName),
            _ => fault.is_none(),
        } && match fault {
            None => true,
            Some(F::InvalidJson | F::JsonLimit | F::DuplicateKey) => {
                matches!(
                    self.form,
                    ArgumentForm::Unparsed | ArgumentForm::NotRetained(_)
                ) && self.tool_operation.is_none()
            }
            Some(F::NotObject) => self.tool_operation.is_none(),
            Some(F::UnknownCommand) => {
                self.tool_operation.is_none()
                    && matches!(
                        tool,
                        Some(
                            RegisteredTool::Project
                                | RegisteredTool::Memory
                                | RegisteredTool::Service
                                | RegisteredTool::Job
                        )
                    )
            }
            Some(F::InvalidJobId) => tool == Some(RegisteredTool::Job),
            Some(
                F::MissingField
                | F::UnexpectedField
                | F::WrongType
                | F::OutOfRange
                | F::UnsafePath
                | F::InvalidVersionId,
            ) => tool.is_some(),
            Some(F::ArgumentsOverLimit) => reason == Some(R::Oversized),
            Some(F::NameOverLimit) => !named,
            Some(F::InvalidText) => true,
            Some(F::UnregisteredName) => named && tool.is_none(),
        } && (received_text
            || decision != D::InvalidArguments
            || fault == Some(F::ArgumentsOverLimit))
            && (reason != Some(R::Oversized)
                || decision != D::InvalidArguments
                || fault.is_some_and(|f| f <= F::ArgumentsOverLimit))
            && (self.form != ArgumentForm::Unparsed
                || decision != D::InvalidArguments
                || matches!(fault, Some(F::InvalidJson | F::JsonLimit | F::DuplicateKey)));
        let decisions = match decision {
            D::Run => {
                self.tool_operation.is_some()
                    && matches!(self.form, ArgumentForm::Verbatim | ArgumentForm::Canonical)
                    && rules
                    && (!exec
                        || (self.form == ArgumentForm::Canonical
                            && (self.whole_command
                                || self.number_kinds.len() <= MAX_EXEC_CMD_VARIABLES)))
            }
            D::UnknownTool => named && received_text,
            D::Busy => exec && self.form == ArgumentForm::Canonical,
            D::Refused => {
                if rules {
                    exec && self.form == ArgumentForm::Canonical && self.refusal.is_some()
                } else {
                    self.refusal.is_none()
                }
            }
            D::InvalidArguments | D::Denied | D::Limit | D::Cancelled => true,
        } && match self.form {
            ArgumentForm::Unparsed | ArgumentForm::NotRetained(R::UnverifiableEscape) => {
                !matches!(decision, D::Run | D::UnknownTool | D::Busy)
            }
            ArgumentForm::NotRetained(R::Oversized) => decision != D::Run,
            _ => true,
        } && match self.reason {
            // J3-E3: every `busy` names its reason; `denied` names
            // `execution_off` only for a parsed exec_cmd call. The codec
            // pairs each code with its decision.
            None => decision != D::Busy,
            Some(reason) => {
                reason.decision() == decision
                    && exec
                    && self.form == ArgumentForm::Canonical
                    && rules
            }
        } && (named
            || matches!(
                decision,
                D::InvalidArguments | D::Refused | D::Limit | D::Cancelled
            ));
        // Rule set: revision 0 only when the self-check failed; then nothing
        // is retained or numbered.
        let rule_set = (self.rule_set_digest == [0; 32]) == !rules
            && if rules {
                true
            } else {
                reason == Some(R::DetectionUnavailable)
                    && !named
                    && self.number_kinds.is_empty()
                    && self.unnumbered == 0
                    && self.tool_operation.is_none()
                    && matches!(decision, D::Refused | D::Limit | D::Cancelled)
            };
        // Fields 20-23 (SL7 amendment): bit 1 marks the whole-command
        // variable, valid only with exec_cmd `run`, and then fields 21-23 hold
        // the fallback reason, never a kept code. `refused` keeps only the
        // kept codes; detection unavailable is recorded without fields.
        let refusal = match self.refusal {
            None => !self.command_cut && !self.whole_command,
            Some(r) => {
                exec && rules
                    && r.code != RefusalCode::DetectionUnavailable
                    && r.word.is_none_or(|w| (1..=MAX_REFUSAL_WORD).contains(&w))
                    && if self.whole_command {
                        decision == D::Run && !r.code.kept()
                    } else {
                        decision == D::Refused && r.code.kept()
                    }
            }
        };
        // Placeholders (TCR-D4, TCR-D20): field 15 is the highest number in
        // fields 5 and 14, every number up to it appears, field 17 counts the
        // unnumbered tokens and no token has a leading zero.
        let name_tokens = asura_platform::secrets::placeholder_tokens(&self.name);
        let text_tokens = asura_platform::secrets::placeholder_tokens(&self.arguments);
        let mut distinct = name_tokens.distinct.clone();
        distinct.extend(&text_tokens.distinct);
        distinct.sort_unstable();
        distinct.dedup();
        let highest = name_tokens.max.max(text_tokens.max) as usize;
        let numbers = !name_tokens.malformed
            && !text_tokens.malformed
            && highest == self.number_kinds.len()
            && distinct.len() == highest
            && name_tokens.zero.checked_add(text_tokens.zero) == Some(self.unnumbered);
        require(shape && operation && faults && decisions && rule_set && refusal && numbers)
    }
}
fn write_tool_call(w: &mut Writer, p: &ToolCall) -> Result<()> {
    if p.arguments.len() > MAX_TOOL_ARGUMENTS || p.name.len() > MAX_STORED_TOOL_NAME {
        return Err(ReplayError::Limit);
    }
    p.validate()?;
    tool_call_fields(w, p);
    Ok(())
}
/// Kind 14 field order. Callers validate first; tests use it to build invalid bytes.
fn tool_call_fields(w: &mut Writer, p: &ToolCall) {
    w.bytes(&p.operation);
    w.u64(p.generation);
    w.u32(p.ordinal);
    w.byte(source_code(p.source));
    w.u32(p.name.len() as u32);
    w.bytes(p.name.as_bytes());
    w.u32(p.name_bytes);
    w.byte(p.tool_operation.map_or(0, |op| op as u8));
    w.byte(p.decision as u8);
    w.byte(p.fault.map_or(0, |f| f as u8));
    w.byte(p.reason.map_or(0, ToolDecisionReason::code));
    w.byte(p.form.code());
    w.byte(p.form.not_retained().map_or(0, |r| r as u8));
    w.u32(p.arguments_bytes);
    w.u32(p.arguments.len() as u32);
    w.bytes(p.arguments.as_bytes());
    w.u16(p.number_kinds.len() as u16);
    for kind in &p.number_kinds {
        w.byte(kind_code(*kind));
    }
    w.u32(p.unnumbered);
    w.u32(p.rule_set_revision);
    w.bytes(&p.rule_set_digest);
    w.byte(u8::from(p.command_cut) | (u8::from(p.whole_command) << 1));
    w.byte(p.refusal.map_or(0, |r| refusal_code(r.code)));
    w.byte(p.refusal.and_then(|r| r.kind).map_or(0, kind_code));
    w.u16(p.refusal.and_then(|r| r.word).unwrap_or(0));
}
fn read_tool_call(r: &mut Reader<'_>) -> Result<ToolCall> {
    let operation = r.array()?;
    let generation = r.u64()?;
    let ordinal = r.u32()?;
    let source = source_from(r.byte()?)?;
    let name = r.bounded_string(MAX_STORED_TOOL_NAME)?;
    let name_bytes = r.u32()?;
    let tool_operation = match r.byte()? {
        0 => None,
        code => Some(ToolOperation::from_code(code).ok_or(ReplayError::Corrupt)?),
    };
    let decision = ToolDecision::from_code(r.byte()?).ok_or(ReplayError::Corrupt)?;
    let fault = match r.byte()? {
        0 => None,
        code => Some(ToolFault::from_code(code).ok_or(ReplayError::Corrupt)?),
    };
    let reason = ToolDecisionReason::from_code(decision, r.byte()?).ok_or(ReplayError::Corrupt)?;
    let form = r.byte()?;
    let form = form_from(form, r.byte()?)?;
    let arguments_bytes = r.u32()?;
    let arguments = r.bounded_string(MAX_TOOL_ARGUMENTS)?;
    let count = r.u16()? as usize;
    require(count <= MAX_TOOL_NUMBERS)?;
    let mut number_kinds = Vec::with_capacity(count);
    for _ in 0..count {
        number_kinds.push(kind_from(r.byte()?)?);
    }
    let unnumbered = r.u32()?;
    let rule_set_revision = r.u32()?;
    let rule_set_digest = r.array()?;
    let flags = r.byte()?;
    require(flags & !0b11 == 0)?;
    let command_cut = flags & 1 != 0;
    let whole_command = flags & 2 != 0;
    let code = r.byte()?;
    let kind = match r.byte()? {
        0 => None,
        code => Some(kind_from(code)?),
    };
    let word = match r.u16()? {
        0 => None,
        word => Some(word),
    };
    let refusal = if code == 0 {
        require(kind.is_none() && word.is_none())?;
        None
    } else {
        Some(ExecCmdRefusal {
            code: refusal_from(code)?,
            kind,
            word,
        })
    };
    let value = ToolCall {
        operation,
        generation,
        ordinal,
        source,
        name,
        name_bytes,
        tool_operation,
        decision,
        fault,
        reason,
        form,
        arguments_bytes,
        arguments,
        number_kinds,
        unnumbered,
        rule_set_revision,
        rule_set_digest,
        command_cut,
        whole_command,
        refusal,
    };
    value.validate()?;
    Ok(value)
}
fn valid_stream(s: &CaptureStream) -> bool {
    s.unnumbered_replacements <= s.replacements
        && s.literal_replacements <= s.replacements
        && s.kinds.len() <= SecretKind::ALL.len()
        && s.kinds.iter().all(|(_, count)| *count > 0)
        && s.kinds
            .windows(2)
            .all(|pair| kind_code(pair[0].0) < kind_code(pair[1].0))
        && s.spans.len() as u64 == s.loss_span_count.min(MAX_CAPTURE_SPANS)
        && s.spans
            .iter()
            .all(|span| span.stored_offset <= s.stored_bytes)
}
/// The capture facts of kind 28 (the rules of the retired kind 23),
/// borrowed for validation.
struct CaptureFacts<'a> {
    state: CaptureState,
    reason: CaptureReason,
    exit_kind: u8,
    exit_value: u32,
    streams: [&'a CaptureStream; 2],
    distinct_numbers: u32,
    numbers: &'a [CaptureNumber],
    final_reservation: u64,
}
/// The capture field rules for kind 28 capture states 1 to 4, unchanged
/// from the retired kind 23 (tool-job-controller.md, kind 28).
fn valid_capture(c: &CaptureFacts<'_>) -> bool {
    let streams = c.streams;
    let digests = streams.iter().all(|s| s.digest.is_some());
    let no_digest = streams.iter().all(|s| s.digest.is_none());
    let state = match c.state {
        CaptureState::Complete => {
            c.reason == CaptureReason::None
                && digests
                && streams.iter().all(|s| s.eof && s.discarded_bytes == 0)
        }
        CaptureState::Incomplete => {
            !matches!(
                c.reason,
                CaptureReason::None | CaptureReason::SettlementUnconfirmed
            ) && digests
        }
        CaptureState::Unconfirmed => c.reason == CaptureReason::SettlementUnconfirmed && no_digest,
        CaptureState::NotStarted => {
            matches!(c.reason, CaptureReason::None | CaptureReason::StorageFault)
                && c.exit_kind == 0
                && no_digest
                && streams
                    .iter()
                    .all(|s| s.observed_bytes == 0 && s.stored_bytes == 0)
        }
    };
    state
        && valid_exit(c.exit_kind, c.exit_value)
        && streams.iter().all(|s| valid_stream(s))
        && c.distinct_numbers <= u32::from(MAX_CAPTURE_NUMBER)
        && c.numbers.len() <= MAX_CAPTURE_NUMBERS
        && c.numbers.len() as u64 <= u64::from(c.distinct_numbers)
        && c.numbers
            .iter()
            .all(|n| (1..=MAX_CAPTURE_NUMBER).contains(&n.number))
        && c.numbers.windows(2).all(|p| p[0].number < p[1].number)
        && c.final_reservation.is_multiple_of(MIB)
}
fn valid_exit(kind: u8, value: u32) -> bool {
    match kind {
        0 => value == 0,
        1 => value <= 255,
        2 => (1..=127).contains(&value),
        _ => false,
    }
}

// ----- J2 kinds 26, 27 and 28. -----

impl JobIntent {
    /// Kind 26 rules that the record alone determines: the agent is 0 (JR3)
    /// and the reference names a valid call or input. Replay adds JR1, JR2
    /// and JR5.
    pub(crate) fn validate(&self) -> Result<()> {
        let reference = match self.origin {
            JobOrigin::Model {
                operation,
                generation,
                ordinal,
            } => {
                operation != [0; 16]
                    && generation >= 1
                    && (1..=MAX_RECORDED_CALLS).contains(&ordinal)
            }
            JobOrigin::Person { input } => input != [0; 16],
        };
        require(self.agent == 0 && reference)
    }
}
impl JobCancelRequested {
    /// Kind 27 rules that the record alone determines: the requester, reason
    /// and kill flag agree with the reason table. Replay adds JR6.
    pub(crate) fn validate(&self) -> Result<()> {
        require(
            self.job != [0; 16]
                && self.reason.requester() == Some(self.requester)
                && self.kill == (self.reason == JobReason::PersonKill),
        )
    }
}
impl JobSettled {
    /// The kind 28 outcome rules and, for capture states 1 to 4, the kind 23
    /// field rules. Replay adds JR7.
    pub(crate) fn validate(&self) -> Result<()> {
        use JobOutcome as O;
        let process = matches!(self.outcome, O::Exited | O::Signalled | O::Cancelled);
        let exit = match self.outcome {
            O::Exited => self.exit_kind == 1,
            O::Signalled => self.exit_kind == 2,
            O::Cancelled => matches!(self.exit_kind, 1 | 2),
            O::NotStarted | O::Interrupted => self.exit_kind == 0,
        } && valid_exit(self.exit_kind, self.exit_value);
        let cleanup = if process {
            self.cleanup != JobCleanup::None
        } else {
            self.cleanup == JobCleanup::None && self.escapees == 0 && self.elapsed_ms == 0
        } && if self.cleanup == JobCleanup::Late {
            matches!(self.unconfirmed_reason, 9 | 13)
        } else {
            self.unconfirmed_reason == 0
        } && (self.cleanup != JobCleanup::None || self.escapees == 0);
        let streams = [&self.stdout, &self.stderr];
        let lines = streams.iter().all(|s| s.lines <= s.capture.stored_bytes);
        let capture = match (self.outcome, self.capture_state.capture()) {
            (O::Interrupted, None) => {
                self.capture_reason == CaptureReason::None
                    && streams.iter().all(|s| **s == JobStream::default())
                    && self.distinct_numbers == 0
                    && self.numbers.is_empty()
                    && self.pause_count == 0
                    && self.first_pause_ms == 0
                    && self.paused_ms == 0
                    && self.final_reservation == 0
            }
            (O::NotStarted, Some(state @ CaptureState::NotStarted))
            | (
                O::Exited | O::Signalled | O::Cancelled,
                Some(
                    state @ (CaptureState::Complete
                    | CaptureState::Incomplete
                    | CaptureState::Unconfirmed),
                ),
            ) => valid_capture(&CaptureFacts {
                state,
                reason: self.capture_reason,
                exit_kind: self.exit_kind,
                exit_value: self.exit_value,
                streams: [&self.stdout.capture, &self.stderr.capture],
                distinct_numbers: self.distinct_numbers,
                numbers: &self.numbers,
                final_reservation: self.final_reservation,
            }),
            _ => false,
        };
        require(
            self.job != [0; 16]
                && self.reason.allows(self.outcome)
                && exit
                && cleanup
                && lines
                && capture,
        )
    }
}
/// Kind 26 field order. Callers validate first; tests use it to build
/// invalid bytes.
fn job_intent_fields(w: &mut Writer, p: &JobIntent) {
    w.byte(p.origin.code());
    w.u32(p.agent);
    match p.origin {
        JobOrigin::Model {
            operation,
            generation,
            ordinal,
        } => {
            w.bytes(&operation);
            w.u64(generation);
            w.u32(ordinal);
        }
        JobOrigin::Person { input } => w.bytes(&input),
    }
    w.u64(p.time);
}
fn read_job_intent(r: &mut Reader<'_>) -> Result<JobIntent> {
    let origin = r.byte()?;
    let agent = r.u32()?;
    let origin = match origin {
        1 => JobOrigin::Model {
            operation: r.array()?,
            generation: r.u64()?,
            ordinal: r.u32()?,
        },
        2 => JobOrigin::Person { input: r.array()? },
        _ => return Err(ReplayError::Corrupt),
    };
    let value = JobIntent {
        origin,
        agent,
        time: r.u64()?,
    };
    value.validate()?;
    Ok(value)
}
/// Kind 27 field order.
fn job_cancel_fields(w: &mut Writer, p: &JobCancelRequested) {
    w.bytes(&p.job);
    w.byte(p.requester as u8);
    w.byte(p.reason as u8);
    w.byte(u8::from(p.kill));
}
fn read_job_cancel(r: &mut Reader<'_>) -> Result<JobCancelRequested> {
    let value = JobCancelRequested {
        job: r.array()?,
        requester: JobRequester::from_code(r.byte()?).ok_or(ReplayError::Corrupt)?,
        reason: JobReason::from_code(r.byte()?).ok_or(ReplayError::Corrupt)?,
        kill: r.flag()?,
    };
    value.validate()?;
    Ok(value)
}
/// Kind 28 field order. Callers validate first; tests use it to build
/// invalid bytes.
fn job_settled_fields(w: &mut Writer, p: &JobSettled) {
    w.bytes(&p.job);
    w.byte(p.outcome as u8);
    w.byte(p.reason as u8);
    w.byte(p.exit_kind);
    w.u32(p.exit_value);
    w.byte(p.cleanup as u8);
    w.byte(p.unconfirmed_reason);
    w.u32(p.escapees);
    w.u64(p.elapsed_ms);
    w.byte(p.capture_state as u8);
    w.byte(p.capture_reason as u8);
    // Field 12, redaction ran: 1, and 0 only for capture state 5.
    w.byte(u8::from(p.capture_state != JobCaptureState::Interrupted));
    for stream in [&p.stdout, &p.stderr] {
        write_stream(w, &stream.capture);
        w.u64(stream.lines);
    }
    w.u32(p.distinct_numbers);
    w.u16(p.numbers.len() as u16);
    for n in &p.numbers {
        w.u16(n.number);
        w.byte(kind_code(n.kind));
        w.byte(origin_code(n.origin));
    }
    w.u32(p.pause_count);
    w.u64(p.first_pause_ms);
    w.u64(p.paused_ms);
    w.u64(p.final_reservation);
    w.u64(p.settled_time);
}
fn read_job_settled(r: &mut Reader<'_>) -> Result<JobSettled> {
    let job = r.array()?;
    let outcome = JobOutcome::from_code(r.byte()?).ok_or(ReplayError::Corrupt)?;
    let reason = JobReason::from_code(r.byte()?).ok_or(ReplayError::Corrupt)?;
    let exit_kind = r.byte()?;
    let exit_value = r.u32()?;
    let cleanup = match r.byte()? {
        0 => JobCleanup::None,
        1 => JobCleanup::Confirmed,
        2 => JobCleanup::Late,
        _ => return Err(ReplayError::Corrupt),
    };
    let unconfirmed_reason = r.byte()?;
    let escapees = r.u32()?;
    let elapsed_ms = r.u64()?;
    let capture_state = match r.byte()? {
        1 => JobCaptureState::Complete,
        2 => JobCaptureState::Incomplete,
        3 => JobCaptureState::Unconfirmed,
        4 => JobCaptureState::NotStarted,
        5 => JobCaptureState::Interrupted,
        _ => return Err(ReplayError::Corrupt),
    };
    let capture_reason = capture_reason_from(r.byte()?)?;
    require(r.byte()? == u8::from(capture_state != JobCaptureState::Interrupted))?;
    let mut stream = || -> Result<JobStream> {
        Ok(JobStream {
            capture: read_stream(r)?,
            lines: r.u64()?,
        })
    };
    let stdout = stream()?;
    let stderr = stream()?;
    let distinct_numbers = r.u32()?;
    let count = r.u16()? as usize;
    require(count <= MAX_CAPTURE_NUMBERS)?;
    let mut numbers = Vec::with_capacity(count);
    for _ in 0..count {
        numbers.push(CaptureNumber {
            number: r.u16()?,
            kind: kind_from(r.byte()?)?,
            origin: origin_from(r.byte()?)?,
        });
    }
    let value = JobSettled {
        job,
        outcome,
        reason,
        exit_kind,
        exit_value,
        cleanup,
        unconfirmed_reason,
        escapees,
        elapsed_ms,
        capture_state,
        capture_reason,
        stdout,
        stderr,
        distinct_numbers,
        numbers,
        pause_count: r.u32()?,
        first_pause_ms: r.u64()?,
        paused_ms: r.u64()?,
        final_reservation: r.u64()?,
        settled_time: r.u64()?,
    };
    value.validate()?;
    Ok(value)
}
fn capture_reason_from(code: u8) -> Result<CaptureReason> {
    Ok(match code {
        0 => CaptureReason::None,
        1 => CaptureReason::StorageFault,
        2 => CaptureReason::Quota,
        3 => CaptureReason::Enospc,
        4 => CaptureReason::DetectorFault,
        5 => CaptureReason::DiscardedAfterCancel,
        6 => CaptureReason::SettlementUnconfirmed,
        _ => return Err(ReplayError::Corrupt),
    })
}
fn require(ok: bool) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(ReplayError::Corrupt)
    }
}
fn write_stream(w: &mut Writer, s: &CaptureStream) {
    w.u64(s.observed_bytes);
    w.u64(s.stored_bytes);
    w.byte(u8::from(s.digest.is_some()));
    w.bytes(&s.digest.unwrap_or([0; 32]));
    w.u64(s.replacements);
    w.u64(s.unnumbered_replacements);
    w.u64(s.literal_replacements);
    w.byte(s.kinds.len() as u8);
    for (kind, count) in &s.kinds {
        w.byte(kind_code(*kind));
        w.u64(*count);
    }
    w.u64(s.loss_span_count);
    w.u64(s.withheld_bytes);
    w.byte(s.spans.len() as u8);
    for span in &s.spans {
        w.u64(span.stored_offset);
        w.u64(span.withheld_bytes);
        w.byte(loss_code(span.reason));
    }
    w.byte(u8::from(s.degraded));
    w.u64(s.discarded_bytes);
    w.byte(u8::from(s.eof));
}
fn read_stream(r: &mut Reader<'_>) -> Result<CaptureStream> {
    let observed_bytes = r.u64()?;
    let stored_bytes = r.u64()?;
    let present = r.flag()?;
    let digest: Hash = r.array()?;
    require(present || digest == [0; 32])?;
    let replacements = r.u64()?;
    let unnumbered_replacements = r.u64()?;
    let literal_replacements = r.u64()?;
    let count = r.byte()? as usize;
    require(count <= SecretKind::ALL.len())?;
    let mut kinds = Vec::with_capacity(count);
    for _ in 0..count {
        kinds.push((kind_from(r.byte()?)?, r.u64()?));
    }
    let loss_span_count = r.u64()?;
    let withheld_bytes = r.u64()?;
    let count = r.byte()? as u64;
    require(count == loss_span_count.min(MAX_CAPTURE_SPANS))?;
    let mut spans = Vec::with_capacity(count as usize);
    for _ in 0..count {
        spans.push(LossSpan {
            stored_offset: r.u64()?,
            withheld_bytes: r.u64()?,
            reason: loss_from(r.byte()?)?,
        });
    }
    Ok(CaptureStream {
        observed_bytes,
        stored_bytes,
        digest: present.then_some(digest),
        replacements,
        unnumbered_replacements,
        literal_replacements,
        kinds,
        loss_span_count,
        withheld_bytes,
        spans,
        degraded: r.flag()?,
        discarded_bytes: r.u64()?,
        eof: r.flag()?,
    })
}
// ----- J3 kind 29 and J4 kind 30. -----

impl JobSnapshot {
    /// State block rules: the reason numbers the state allows; elapsed 0
    /// before spawn and when settled; zero bytes when settled.
    fn valid(&self) -> bool {
        self.state.allows_numbers(self.numbers)
            && (!self.state.before_spawn() || self.elapsed_ms == 0)
            && (self.state != JobRunState::Settled
                || (self.elapsed_ms == 0 && self.stdout_bytes == 0 && self.stderr_bytes == 0))
    }
}
impl JobListEntry {
    /// A list entry follows the state block rules for state, reason and
    /// numbers; a settled entry stores zero bytes.
    fn valid(&self) -> bool {
        self.job != [0; 16]
            && self.state.allows_numbers(self.numbers)
            && (self.state != JobRunState::Settled
                || (self.stdout_bytes == 0 && self.stderr_bytes == 0))
    }
}
impl JobPage {
    /// Page block rules: at most 8,192 source bytes; none for states 5 to
    /// 8; a digest exactly when bytes were read; the bytes lie within the
    /// stored stream.
    fn valid(&self) -> bool {
        self.source_len <= MAX_JOB_PAGE
            && (self.state.readable() || self.source_len == 0)
            && (self.digest == [0; 32]) == (self.source_len == 0)
            && (self.source_len == 0
                || self
                    .start
                    .checked_add(u64::from(self.source_len))
                    .is_some_and(|end| end <= self.total))
    }
}
impl JobResult {
    /// Every kind 29 rule that the record alone determines: the status
    /// fixed by the outcome, the body forms an outcome allows, and each
    /// block's rules. Replay adds JR11 to JR16 with the operation code.
    pub(crate) fn validate(&self) -> Result<()> {
        use JobResultOutcome as O;
        let body = match &self.body {
            JobResultBody::None => true,
            JobResultBody::State(s) => matches!(self.outcome, O::Ok | O::Cancelled) && s.valid(),
            JobResultBody::Wait(s, end) => {
                self.outcome == O::Ok
                    && s.valid()
                    && (!matches!(end, WaitEnd::Settled | WaitEnd::AlreadySettled)
                        || s.state == JobRunState::Settled)
            }
            JobResultBody::Page(p) => self.outcome == O::Ok && p.valid(),
            JobResultBody::List { more, entries } => {
                let mut jobs: Vec<Id> = entries.iter().map(|e| e.job).collect();
                jobs.sort_unstable();
                jobs.dedup();
                self.outcome == O::Ok
                    && self.job == [0; 16]
                    && entries.len() <= MAX_JOB_LIST_ENTRIES
                    && (!more || entries.len() == MAX_JOB_LIST_ENTRIES)
                    && jobs.len() == entries.len()
                    && entries.iter().all(JobListEntry::valid)
            }
        };
        require(
            self.operation != [0; 16]
                && self.generation >= 1
                && (1..=MAX_RECORDED_CALLS).contains(&self.ordinal)
                && self.status == self.outcome.status()
                && (self.outcome != O::NotFound || self.job == [0; 16])
                && body,
        )
    }
}
fn write_snapshot(w: &mut Writer, s: &JobSnapshot) {
    w.byte(s.state.code());
    w.byte(s.state.reason_code());
    w.bytes(&s.numbers);
    w.u64(s.elapsed_ms);
    w.u64(s.stdout_bytes);
    w.u64(s.stderr_bytes);
}
fn read_run_state(r: &mut Reader<'_>) -> Result<JobRunState> {
    let state = r.byte()?;
    JobRunState::from_codes(state, r.byte()?).ok_or(ReplayError::Corrupt)
}
fn read_snapshot(r: &mut Reader<'_>) -> Result<JobSnapshot> {
    Ok(JobSnapshot {
        state: read_run_state(r)?,
        numbers: r.array()?,
        elapsed_ms: r.u64()?,
        stdout_bytes: r.u64()?,
        stderr_bytes: r.u64()?,
    })
}
/// Kind 29 field order. Callers validate first; tests use it to build
/// invalid bytes.
fn job_result_fields(w: &mut Writer, p: &JobResult) {
    w.bytes(&p.operation);
    w.u64(p.generation);
    w.u32(p.ordinal);
    w.byte(p.status);
    w.byte(p.outcome as u8);
    w.bytes(&p.job);
    match &p.body {
        JobResultBody::None => {}
        JobResultBody::State(s) => write_snapshot(w, s),
        JobResultBody::Wait(s, end) => {
            write_snapshot(w, s);
            w.byte(*end as u8);
        }
        JobResultBody::Page(page) => {
            w.byte(page.stream as u8);
            w.u64(page.start);
            w.u32(page.source_len);
            w.u64(page.total);
            w.byte(page.state as u8);
            w.bytes(&page.digest);
        }
        JobResultBody::List { more, entries } => {
            w.byte(entries.len() as u8);
            w.byte(u8::from(*more));
            for e in entries {
                w.bytes(&e.job);
                w.byte(e.state.code());
                w.byte(e.state.reason_code());
                w.bytes(&e.numbers);
                w.u64(e.stdout_bytes);
                w.u64(e.stderr_bytes);
            }
        }
    }
}
fn read_job_result(r: &mut Reader<'_>) -> Result<JobResult> {
    let operation = r.array()?;
    let generation = r.u64()?;
    let ordinal = r.u32()?;
    let status = r.byte()?;
    let outcome = JobResultOutcome::from_code(r.byte()?).ok_or(ReplayError::Corrupt)?;
    let job = r.array()?;
    // The body form is read by its size; the sizes are distinct.
    let body = match r.bytes.len() - r.offset {
        0 => JobResultBody::None,
        JOB_STATE_BLOCK => JobResultBody::State(read_snapshot(r)?),
        JOB_WAIT_BLOCK => {
            let snapshot = read_snapshot(r)?;
            JobResultBody::Wait(
                snapshot,
                WaitEnd::from_code(r.byte()?).ok_or(ReplayError::Corrupt)?,
            )
        }
        JOB_PAGE_BLOCK => JobResultBody::Page(JobPage {
            stream: OutputStream::from_code(r.byte()?).ok_or(ReplayError::Corrupt)?,
            start: r.u64()?,
            source_len: r.u32()?,
            total: r.u64()?,
            state: JobPageState::from_code(r.byte()?).ok_or(ReplayError::Corrupt)?,
            digest: r.array()?,
        }),
        size => {
            let count = r.byte()? as usize;
            require(count <= MAX_JOB_LIST_ENTRIES && size == 2 + count * JOB_LIST_ENTRY)?;
            let more = r.flag()?;
            let mut entries = Vec::with_capacity(count);
            for _ in 0..count {
                entries.push(JobListEntry {
                    job: r.array()?,
                    state: read_run_state(r)?,
                    numbers: r.array()?,
                    stdout_bytes: r.u64()?,
                    stderr_bytes: r.u64()?,
                });
            }
            JobResultBody::List { more, entries }
        }
    };
    let value = JobResult {
        operation,
        generation,
        ordinal,
        status,
        outcome,
        job,
        body,
    };
    value.validate()?;
    Ok(value)
}
impl JobRetention {
    /// Every kind 30 field rule that the record alone determines. Replay
    /// adds the job, the model reference and the pin and delete state.
    pub(crate) fn validate(&self) -> Result<()> {
        let author = match self.author {
            RetentionAuthor::Person { request } => {
                request != [0; 16]
                    && if self.action == RetentionAction::Note {
                        (1..=MAX_JOB_NOTE).contains(&self.note.len())
                    } else {
                        self.note.is_empty()
                    }
            }
            // The model writes only `note`; its text is its kind 14's.
            RetentionAuthor::Model {
                operation,
                generation,
                ordinal,
            } => {
                self.action == RetentionAction::Note
                    && self.note.is_empty()
                    && operation != [0; 16]
                    && generation >= 1
                    && (1..=MAX_RECORDED_CALLS).contains(&ordinal)
            }
        };
        require(self.job != [0; 16] && author)
    }
}
/// Kind 30 field order. Callers validate first; tests use it to build
/// invalid bytes.
fn job_retention_fields(w: &mut Writer, p: &JobRetention) {
    w.bytes(&p.job);
    w.byte(p.action as u8);
    w.byte(p.author.code());
    w.u64(p.time);
    match p.author {
        RetentionAuthor::Person { request } => w.bytes(&request),
        RetentionAuthor::Model {
            operation,
            generation,
            ordinal,
        } => {
            w.bytes(&operation);
            w.u64(generation);
            w.u32(ordinal);
        }
    }
    w.u32(p.note.len() as u32);
    w.bytes(p.note.as_bytes());
}
fn read_job_retention(r: &mut Reader<'_>) -> Result<JobRetention> {
    let job = r.array()?;
    let action = RetentionAction::from_code(r.byte()?).ok_or(ReplayError::Corrupt)?;
    let author = r.byte()?;
    let time = r.u64()?;
    let author = match author {
        1 => RetentionAuthor::Person {
            request: r.array()?,
        },
        2 => RetentionAuthor::Model {
            operation: r.array()?,
            generation: r.u64()?,
            ordinal: r.u32()?,
        },
        _ => return Err(ReplayError::Corrupt),
    };
    let value = JobRetention {
        job,
        action,
        author,
        time,
        note: r.bounded_string(MAX_JOB_NOTE)?,
    };
    value.validate()?;
    Ok(value)
}

fn payload(record: &Record) -> Result<Vec<u8>> {
    let mut w = Writer(Vec::new());
    match record {
        Record::InputQueued(p) => {
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
            if p.prior_operations.len() > MAX_COMPOSED_PRIOR {
                return Err(ReplayError::Limit);
            }
            w.bytes(&(p.prior_operations.len() as u16).to_be_bytes());
            for id in &p.prior_operations {
                w.bytes(id);
            }
            w.string(&p.prompt, MAX_PROMPT)?;
            w.u64(p.event_cursor);
            let c = &p.composition;
            w.u32(c.omitted_turns);
            w.bytes(&c.manifest_digest);
            w.u32(c.composer_revision);
            w.u32(c.window_tokens);
            w.u32(c.output_reservation);
            w.u32(c.overhead_tokens);
            w.u32(c.bytes_per_token_milli);
            require(
                p.thinking_level <= MAX_THINKING_LEVEL
                    && p.thinking_request <= MAX_THINKING_REQUEST,
            )?;
            w.byte(p.thinking_level);
            w.byte(p.thinking_request);
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
        Record::ToolCall(p) => write_tool_call(&mut w, p)?,
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
            w.u64(p.text_offset);
            w.string(&p.text, MAX_TEXT)?;
        }

        Record::JobIntent(p) => {
            p.validate()?;
            job_intent_fields(&mut w, p);
        }
        Record::JobCancelRequested(p) => {
            p.validate()?;
            job_cancel_fields(&mut w, p);
        }
        Record::JobSettled(p) => {
            p.validate()?;
            job_settled_fields(&mut w, p);
        }
        Record::JobResult(p) => {
            p.validate()?;
            job_result_fields(&mut w, p);
        }
        Record::JobRetention(p) => {
            if p.note.len() > MAX_JOB_NOTE {
                return Err(ReplayError::Limit);
            }
            p.validate()?;
            job_retention_fields(&mut w, p);
        }
        Record::ResponseText(p) | Record::ThinkingText(p) => {
            require(!p.text.is_empty() || p.offset > 0)?;
            w.bytes(&p.operation);
            w.u64(p.generation);
            w.u64(p.offset);
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
        18 => Record::InputQueued(InputQueued {
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
        21 => {
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
            if count > MAX_COMPOSED_PRIOR {
                return Err(ReplayError::Limit);
            }
            let mut prior_operations = Vec::with_capacity(count);
            for _ in 0..count {
                prior_operations.push(r.array()?);
            }
            let prompt = r.string(MAX_PROMPT)?;
            let event_cursor = r.u64()?;
            let composition = Composition {
                omitted_turns: r.u32()?,
                manifest_digest: r.array()?,
                composer_revision: r.u32()?,
                window_tokens: r.u32()?,
                output_reservation: r.u32()?,
                overhead_tokens: r.u32()?,
                bytes_per_token_milli: r.u32()?,
            };
            let thinking_level = r.byte()?;
            let thinking_request = r.byte()?;
            require(
                thinking_level <= MAX_THINKING_LEVEL && thinking_request <= MAX_THINKING_REQUEST,
            )?;
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
                prompt,
                event_cursor,
                composition,
                thinking_level,
                thinking_request,
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
            kind: TerminalKind::from_code(r.byte()?).ok_or(ReplayError::Corrupt)?,
            cause: r.cause()?,
            final_cursor: r.u64()?,
            usage_known: r.flag()?,
            output_tokens: r.u32()?,
            text_offset: r.u64()?,
            text: r.string(MAX_TEXT)?,
        }),
        24 => {
            let value = ResponseText {
                operation: r.array()?,
                generation: r.u64()?,
                offset: r.u64()?,
                text: r.string(MAX_TEXT)?,
            };
            require(!value.text.is_empty() || value.offset > 0)?;
            Record::ResponseText(value)
        }
        25 => {
            let value = ResponseText {
                operation: r.array()?,
                generation: r.u64()?,
                offset: r.u64()?,
                text: r.string(MAX_TEXT)?,
            };
            require(!value.text.is_empty() || value.offset > 0)?;
            Record::ThinkingText(value)
        }
        14 => {
            require(payload.len() <= MAX_TOOL_CALL_PAYLOAD)?;
            Record::ToolCall(Box::new(read_tool_call(&mut r)?))
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
        // Kinds 17 and 22 are retired (TCR-O2), and kind 23 with the J1
        // exec_cmd path (J3-E9, JR17); like any unknown kind they are
        // UnsupportedFormat.
        26 => {
            require(payload.len() <= MAX_JOB_INTENT_PAYLOAD)?;
            Record::JobIntent(read_job_intent(&mut r)?)
        }
        27 => {
            require(payload.len() == JOB_CANCEL_PAYLOAD)?;
            Record::JobCancelRequested(read_job_cancel(&mut r)?)
        }
        28 => {
            require(payload.len() <= MAX_JOB_SETTLED_PAYLOAD)?;
            Record::JobSettled(Box::new(read_job_settled(&mut r)?))
        }
        29 => {
            require(payload.len() <= MAX_JOB_RESULT_PAYLOAD)?;
            Record::JobResult(Box::new(read_job_result(&mut r)?))
        }
        30 => {
            require(payload.len() <= MAX_JOB_RETENTION_PAYLOAD)?;
            Record::JobRetention(Box::new(read_job_retention(&mut r)?))
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
            text_offset: 0,
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
    /// J2T-04 (storage part): one `asura.job` domain with an origin byte,
    /// fixed vectors for both origins, the zero rule and each input change;
    /// a J1 call uses the same model-origin derivation; payload IDs keyed by
    /// job ID. Vectors were computed independently (Python `hashlib`).
    #[test]
    fn j2t_04_job_and_payload_identity_vectors() {
        let model = JobOrigin::Model {
            operation: [1; 16],
            generation: 1,
            ordinal: 1,
        };
        let model_id = [
            0x4f, 0xdf, 0x69, 0x57, 0xba, 0x94, 0xa9, 0x20, 0xdf, 0x3a, 0x19, 0x62, 0x7d, 0x23,
            0x17, 0x43,
        ];
        assert_eq!(job_id(&model), model_id);
        assert_eq!(
            exec_cmd_job_id([1; 16], 1, 1),
            model_id,
            "J1 calls share it"
        );
        let person = JobOrigin::Person { input: [7; 16] };
        assert_eq!(
            job_id(&person),
            [
                0x79, 0xe0, 0xdb, 0x74, 0x2b, 0x4e, 0x16, 0x9a, 0x46, 0x80, 0x08, 0x6f, 0x7a, 0xa9,
                0x2c, 0x65,
            ]
        );
        for other in [
            JobOrigin::Model {
                operation: [2; 16],
                generation: 1,
                ordinal: 1,
            },
            JobOrigin::Model {
                operation: [1; 16],
                generation: 2,
                ordinal: 1,
            },
            JobOrigin::Model {
                operation: [1; 16],
                generation: 1,
                ordinal: 2,
            },
            JobOrigin::Person { input: [1; 16] },
        ] {
            assert_ne!(job_id(&other), model_id, "{other:?}");
        }
        assert_ne!(
            job_id(&JobOrigin::Person { input: [8; 16] }),
            job_id(&person)
        );
        // The origin byte separates origins whose references share bytes.
        let mut reference = Sha256::new();
        reference.update(b"asura.job\0");
        reference.update([2]);
        reference.update([1; 16]);
        assert_eq!(
            derived_id(reference),
            job_id(&JobOrigin::Person { input: [1; 16] })
        );
        // Zero rule: an all-zero prefix becomes 1 in its final byte.
        let mut zero = [0; 16];
        zero[15] = 1;
        assert_eq!(id_from_digest(&[0; 32]), zero);
        assert_eq!(id_from_digest(&[0xab; 32]), [0xab; 16]);
        // Payload IDs: installation, job ID and object kind.
        let payloads = job_payload_ids([9; 16], model_id);
        assert_eq!(
            payloads,
            [
                [
                    0x23, 0x08, 0xfb, 0x33, 0x95, 0xb6, 0x5a, 0x87, 0xbf, 0x96, 0x8a, 0xfb, 0xbd,
                    0xa5, 0x41, 0x62,
                ],
                [
                    0xa3, 0x50, 0xcc, 0xc2, 0x95, 0xc3, 0x07, 0x9f, 0x57, 0xab, 0xcf, 0x7e, 0x99,
                    0x1d, 0x97, 0xf7,
                ],
            ]
        );
        for other in [
            job_payload_ids([8; 16], model_id),
            job_payload_ids([9; 16], job_id(&person)),
        ] {
            assert!(other.iter().all(|id| !payloads.contains(id)));
        }
    }

    // TCR-U01..U03: kind 14 ToolCall.
    fn run14() -> ToolCall {
        ToolCall {
            operation: [1; 16],
            generation: 1,
            ordinal: 1,
            source: ToolCallSource::GeneratedContent,
            name: "exec_cmd".into(),
            name_bytes: 8,
            tool_operation: Some(ToolOperation::ExecCmd),
            decision: ToolDecision::Run,
            fault: None,
            reason: None,
            form: ArgumentForm::Canonical,
            arguments_bytes: 48,
            arguments: "{\"command\":\"printf \\\"$ASURA_SECRET_1\\\"\"}".into(),
            number_kinds: vec![SecretKind::GithubToken],
            unnumbered: 0,
            rule_set_revision: 1,
            rule_set_digest: [5; 32],
            command_cut: false,
            whole_command: false,
            refusal: None,
        }
    }
    fn refused14() -> ToolCall {
        ToolCall {
            decision: ToolDecision::Refused,
            arguments: "{\"command\":\"ls\",\"cwd\":\"dir/$ASURA_SECRET_1\"}".into(),
            refusal: Some(ExecCmdRefusal {
                code: RefusalCode::SecretInCwd,
                kind: Some(SecretKind::GithubToken),
                word: None,
            }),
            ..run14()
        }
    }
    /// SL7: a `run` exec_cmd through the whole-command variable.
    fn whole14() -> ToolCall {
        ToolCall {
            arguments: "{\"command\":\"echo '$ASURA_SECRET_1'\"}".into(),
            whole_command: true,
            refusal: Some(ExecCmdRefusal {
                code: RefusalCode::SecretSingleQuoted,
                kind: Some(SecretKind::GithubToken),
                word: Some(2),
            }),
            ..run14()
        }
    }
    fn invalid14() -> ToolCall {
        ToolCall {
            name: "project".into(),
            name_bytes: 7,
            tool_operation: Some(ToolOperation::ReadFile),
            decision: ToolDecision::InvalidArguments,
            fault: Some(ToolFault::MissingField),
            form: ArgumentForm::Verbatim,
            arguments_bytes: 24,
            arguments: "{\"command\":\"read_file\"}".into(),
            number_kinds: vec![],
            ..run14()
        }
    }
    fn raw14(p: &ToolCall) -> Vec<u8> {
        let mut w = Writer(Vec::new());
        tool_call_fields(&mut w, p);
        w.0
    }
    fn round14(p: ToolCall) -> Vec<u8> {
        let record = Record::ToolCall(Box::new(p));
        let bytes = payload(&record).unwrap();
        assert_eq!(
            bytes,
            raw14(match &record {
                Record::ToolCall(p) => p,
                _ => unreachable!(),
            })
        );
        assert_eq!(decode_record(14, &bytes).unwrap(), record);
        bytes
    }
    fn invalid14_record(p: ToolCall) {
        let record = Record::ToolCall(Box::new(p.clone()));
        assert!(payload(&record).is_err(), "{p:?}");
        assert_eq!(
            decode_record(14, &raw14(&p)),
            Err(ReplayError::Corrupt),
            "{p:?}"
        );
    }
    fn tokens(count: usize) -> String {
        (1..=count).map(|n| format!("$ASURA_SECRET_{n} ")).collect()
    }
    /// SL7 amendment: replay derives the fixed `eval` command and one secret
    /// variable when field 20 bit 1 is set, whatever the stored numbers;
    /// without it the stored command is the executable text and each number
    /// is a variable.
    #[test]
    fn sl7_replay_derives_whole_command_and_one_variable() {
        use super::super::state::exec_cmd_call;
        let whole = exec_cmd_call([9; 16], &whole14(), 7);
        assert_eq!(whole.fixed_command, Some(WHOLE_COMMAND));
        assert_eq!(whole.fixed_command, Some("eval \"$ASURA_SECRET_COMMAND\""));
        assert_eq!(whole.secret_variables, 1);
        let many = ToolCall {
            arguments: format!("{{\"command\":\"{}\"}}", tokens(17)),
            number_kinds: vec![SecretKind::Jwt; 17],
            ..whole14()
        };
        assert_eq!(exec_cmd_call([9; 16], &many, 7).secret_variables, 1);
        let rewritten = exec_cmd_call([9; 16], &run14(), 7);
        assert_eq!(
            (rewritten.fixed_command, rewritten.secret_variables),
            (None, 1)
        );
        let plain = ToolCall {
            arguments: "{\"command\":\"ls\"}".into(),
            number_kinds: vec![],
            ..run14()
        };
        assert_eq!(exec_cmd_call([9; 16], &plain, 7).secret_variables, 0);
        // The derived job and payload identities do not depend on bit 1.
        assert_eq!(whole.job, exec_cmd_call([9; 16], &run14(), 7).job);
    }
    /// TCR-U01: every field at each bound round trips through the real
    /// encoder and decoder.
    #[test]
    fn tcr_u01_kind_14_round_trips_every_field_bound() {
        round14(run14());
        round14(refused14());
        round14(invalid14());
        let bytes = round14(whole14());
        assert_eq!(bytes[bytes.len() - 5], 2, "field 20 bit 1");
        // Every recorded ordinal round trips with any decision (owner, 2026-10-18).
        for ordinal in 1..=MAX_RECORDED_CALLS {
            round14(ToolCall { ordinal, ..run14() });
        }
        round14(ToolCall {
            ordinal: MAX_TOOL_CALL_ORDINAL,
            decision: ToolDecision::Limit,
            ..run14()
        });
        // After cancel or turn end, the over-limit ordinal is `cancelled` (TCR-D30).
        round14(ToolCall {
            ordinal: MAX_TOOL_CALL_ORDINAL,
            decision: ToolDecision::Cancelled,
            ..run14()
        });
        round14(ToolCall {
            generation: u64::MAX,
            source: ToolCallSource::ProviderJson,
            rule_set_revision: u32::MAX,
            ..run14()
        });
        // Name not retained: empty name, any received size, operation none.
        round14(ToolCall {
            name: String::new(),
            name_bytes: u32::MAX,
            tool_operation: None,
            decision: ToolDecision::InvalidArguments,
            fault: Some(ToolFault::NameOverLimit),
            form: ArgumentForm::Verbatim,
            arguments: "{}".into(),
            number_kinds: vec![],
            ..run14()
        });
        // The longest stored name (redaction lengthened a received name).
        let long = ToolCall {
            name: "n".repeat(MAX_STORED_TOOL_NAME),
            name_bytes: MAX_TOOL_NAME as u32,
            tool_operation: None,
            decision: ToolDecision::UnknownTool,
            fault: Some(ToolFault::UnregisteredName),
            form: ArgumentForm::Verbatim,
            arguments: String::new(),
            arguments_bytes: 0,
            number_kinds: vec![],
            ..run14()
        };
        round14(long.clone());
        // Every decision with a consistent shape.
        for (decision, fault, reason) in [
            (
                ToolDecision::InvalidArguments,
                Some(ToolFault::WrongType),
                None,
            ),
            (ToolDecision::Denied, None, None),
            (ToolDecision::Limit, None, None),
            (ToolDecision::Cancelled, None, None),
        ] {
            round14(ToolCall {
                decision,
                fault,
                reason,
                ..run14()
            });
        }
        // J3T-14: field 10, the decision reason (J3-E3).
        for (decision, reason, code) in [
            (ToolDecision::Busy, ToolDecisionReason::QueueFull, 1),
            (ToolDecision::Busy, ToolDecisionReason::JobsFull, 2),
            (ToolDecision::Busy, ToolDecisionReason::Closed, 3),
            (ToolDecision::Denied, ToolDecisionReason::ExecutionOff, 1),
        ] {
            let bytes = round14(ToolCall {
                decision,
                reason: Some(reason),
                ..run14()
            });
            assert_eq!(bytes[48], code);
        }
        // The `job` tool and fault 16 (J3T-01, J3-E2).
        for op in [
            ToolOperation::JobList,
            ToolOperation::JobStatus,
            ToolOperation::JobRead,
            ToolOperation::JobWait,
            ToolOperation::JobCancel,
            ToolOperation::JobNote,
        ] {
            let bytes = round14(ToolCall {
                name: "job".into(),
                name_bytes: 3,
                tool_operation: Some(op),
                form: ArgumentForm::Verbatim,
                arguments: "{\"command\":\"list\"}".into(),
                arguments_bytes: 18,
                number_kinds: vec![],
                ..run14()
            });
            assert_eq!(bytes[40], op as u8);
        }
        for fault in [ToolFault::InvalidJobId, ToolFault::UnknownCommand] {
            round14(ToolCall {
                name: "job".into(),
                name_bytes: 3,
                tool_operation: None,
                decision: ToolDecision::InvalidArguments,
                fault: Some(fault),
                form: ArgumentForm::Verbatim,
                arguments: "{\"command\":\"status\",\"job\":\"0\"}".into(),
                arguments_bytes: 30,
                number_kinds: vec![],
                ..run14()
            });
        }
        // Every form and not-retained reason.
        for form in [
            ArgumentForm::Unparsed,
            ArgumentForm::NotRetained(NotRetainedReason::UnverifiableEscape),
        ] {
            round14(ToolCall {
                tool_operation: None,
                fault: Some(ToolFault::InvalidJson),
                form,
                arguments: if form == ArgumentForm::Unparsed {
                    "{\"command\"".into()
                } else {
                    String::new()
                },
                ..invalid14()
            });
        }
        round14(ToolCall {
            tool_operation: None,
            fault: Some(ToolFault::ArgumentsOverLimit),
            form: ArgumentForm::NotRetained(NotRetainedReason::Oversized),
            arguments_bytes: u32::MAX,
            arguments: String::new(),
            ..invalid14()
        });
        // Rule set unavailable: nothing retained, decision refused.
        round14(ToolCall {
            name: String::new(),
            tool_operation: None,
            decision: ToolDecision::Refused,
            form: ArgumentForm::NotRetained(NotRetainedReason::DetectionUnavailable),
            arguments: String::new(),
            number_kinds: vec![],
            rule_set_revision: 0,
            rule_set_digest: [0; 32],
            ..run14()
        });
        // Refusal bounds and the cut flag, refused and whole-command.
        round14(ToolCall {
            command_cut: true,
            refusal: Some(ExecCmdRefusal {
                code: RefusalCode::PlaceholderNotAvailable,
                kind: None,
                word: Some(4096),
            }),
            ..refused14()
        });
        let bytes = round14(ToolCall {
            command_cut: true,
            refusal: Some(ExecCmdRefusal {
                code: RefusalCode::RewrittenCommandTooLong,
                kind: None,
                word: Some(4096),
            }),
            ..whole14()
        });
        assert_eq!(bytes[bytes.len() - 5], 3, "field 20 bits 0 and 1");
        // A whole-command run may hold more than 16 numbers (SO5).
        round14(ToolCall {
            arguments: format!("{{\"command\":\"{}\"}}", tokens(17)),
            number_kinds: vec![SecretKind::Jwt; 17],
            refusal: Some(ExecCmdRefusal {
                code: RefusalCode::TooManySecrets,
                kind: None,
                word: None,
            }),
            ..whole14()
        });
        // Numbers: 256 numbered placeholders and unnumbered tokens.
        let text = format!(
            "{{\"body\":\"{}$ASURA_SECRET_0 ${{ASURA_SECRET_0}}\"}}",
            tokens(256)
        );
        round14(ToolCall {
            name: "memory".into(),
            name_bytes: 6,
            tool_operation: Some(ToolOperation::CreateNote),
            arguments: text,
            number_kinds: (0..256).map(|n| SecretKind::ALL[n % 33]).collect(),
            unnumbered: 2,
            ..run14()
        });
        // A placeholder in the name counts too.
        round14(ToolCall {
            name: "x$ASURA_SECRET_1".into(),
            arguments: "{}".into(),
            number_kinds: vec![SecretKind::Jwt],
            ..long
        });
    }
    /// TCR-U01: each out-of-range value, unknown code and inconsistent
    /// combination is Corrupt.
    #[test]
    fn tcr_u01_kind_14_rejects_each_invalid_field() {
        use ToolDecision as D;
        use ToolFault as F;
        for p in [
            ToolCall {
                operation: [0; 16],
                ..run14()
            },
            ToolCall {
                generation: 0,
                ..run14()
            },
            ToolCall {
                ordinal: 0,
                ..run14()
            },
            ToolCall {
                ordinal: MAX_TOOL_CALL_ORDINAL + 1,
                ..run14()
            },
            // The over-limit ordinal only with limit or cancelled.
            ToolCall {
                ordinal: MAX_TOOL_CALL_ORDINAL,
                ..run14()
            },
            ToolCall {
                name_bytes: 0,
                ..run14()
            },
            // A retained name was received at most 256 bytes.
            ToolCall {
                name_bytes: 257,
                ..run14()
            },
            // Operation must match the registry lookup of the name.
            ToolCall {
                tool_operation: Some(ToolOperation::ReadFile),
                ..run14()
            },
            ToolCall {
                name: "unknown".into(),
                ..run14()
            },
            // Run needs an operation, a parsed form and a rule set.
            ToolCall {
                tool_operation: None,
                ..run14()
            },
            ToolCall {
                form: ArgumentForm::Unparsed,
                ..run14()
            },
            // Faults only with invalid_arguments or unknown_tool.
            ToolCall {
                fault: Some(F::MissingField),
                ..run14()
            },
            ToolCall {
                fault: None,
                ..invalid14()
            },
            ToolCall {
                fault: Some(F::UnregisteredName),
                ..invalid14()
            },
            ToolCall {
                decision: D::UnknownTool,
                ..invalid14()
            },
            // JSON faults need unparsed (or not retained) text and no operation.
            ToolCall {
                fault: Some(F::InvalidJson),
                ..invalid14()
            },
            // Unknown command needs a grouped registered tool and no operation.
            ToolCall {
                fault: Some(F::UnknownCommand),
                ..invalid14()
            },
            ToolCall {
                fault: Some(F::ArgumentsOverLimit),
                ..invalid14()
            },
            ToolCall {
                fault: Some(F::NameOverLimit),
                ..invalid14()
            },
            // Field 10: a busy reason only with busy, `execution_off` only
            // with denied, both only for a parsed exec_cmd call; busy always
            // names its reason (J3-E3).
            ToolCall {
                reason: Some(ToolDecisionReason::QueueFull),
                ..run14()
            },
            ToolCall {
                reason: Some(ToolDecisionReason::ExecutionOff),
                ..run14()
            },
            ToolCall {
                decision: D::Busy,
                ..run14()
            },
            ToolCall {
                decision: D::Denied,
                reason: Some(ToolDecisionReason::Closed),
                ..run14()
            },
            ToolCall {
                decision: D::Busy,
                reason: Some(ToolDecisionReason::QueueFull),
                ..invalid14()
            },
            ToolCall {
                decision: D::Denied,
                reason: Some(ToolDecisionReason::ExecutionOff),
                ..invalid14()
            },
            ToolCall {
                decision: D::Denied,
                reason: Some(ToolDecisionReason::ExecutionOff),
                name: "job".into(),
                name_bytes: 3,
                tool_operation: Some(ToolOperation::JobList),
                ..run14()
            },
            // Fault 16 only for the `job` tool.
            ToolCall {
                fault: Some(F::InvalidJobId),
                ..invalid14()
            },
            // A job operation code needs the `job` name.
            ToolCall {
                tool_operation: Some(ToolOperation::JobList),
                ..run14()
            },
            // Form 4 holds no text; forms 1-3 at most 64,000 bytes.
            ToolCall {
                form: ArgumentForm::NotRetained(NotRetainedReason::Oversized),
                fault: Some(F::ArgumentsOverLimit),
                ..invalid14()
            },
            // Text over the limit was received only as a size.
            ToolCall {
                arguments_bytes: 64_001,
                ..invalid14()
            },
            // Rule set: revision and digest are zero together, and zero only
            // with detection_unavailable.
            ToolCall {
                rule_set_revision: 0,
                ..run14()
            },
            ToolCall {
                rule_set_digest: [0; 32],
                ..run14()
            },
            // Refusal only on a refused exec_cmd; never detection unavailable.
            ToolCall {
                refusal: refused14().refusal,
                ..run14()
            },
            ToolCall {
                refusal: None,
                ..refused14()
            },
            ToolCall {
                refusal: Some(ExecCmdRefusal {
                    code: RefusalCode::DetectionUnavailable,
                    kind: None,
                    word: None,
                }),
                ..refused14()
            },
            ToolCall {
                refusal: Some(ExecCmdRefusal {
                    code: RefusalCode::SecretSingleQuoted,
                    kind: None,
                    word: Some(4097),
                }),
                ..refused14()
            },
            ToolCall {
                command_cut: true,
                ..run14()
            },
            // SL7: bit 1 only with exec_cmd `run` and a fallback reason that
            // is not a kept code; `refused` only with a kept code.
            ToolCall {
                decision: ToolDecision::Refused,
                ..whole14()
            },
            ToolCall {
                decision: ToolDecision::Busy,
                reason: Some(ToolDecisionReason::QueueFull),
                ..whole14()
            },
            ToolCall {
                decision: ToolDecision::Denied,
                ..whole14()
            },
            ToolCall {
                decision: ToolDecision::Cancelled,
                ..whole14()
            },
            ToolCall {
                refusal: None,
                ..whole14()
            },
            ToolCall {
                refusal: refused14().refusal,
                ..whole14()
            },
            ToolCall {
                refusal: Some(ExecCmdRefusal {
                    code: RefusalCode::PlaceholderNotAvailable,
                    kind: None,
                    word: None,
                }),
                ..whole14()
            },
            ToolCall {
                name: "project".into(),
                name_bytes: 7,
                tool_operation: Some(ToolOperation::ReadFile),
                form: ArgumentForm::Verbatim,
                ..whole14()
            },
            ToolCall {
                whole_command: true,
                refusal: None,
                ..run14()
            },
            ToolCall {
                refusal: whole14().refusal,
                ..refused14()
            },
            // Placeholder counts (TCR-D4, TCR-D20).
            ToolCall {
                number_kinds: vec![],
                ..run14()
            },
            ToolCall {
                number_kinds: vec![SecretKind::Jwt; 2],
                ..run14()
            },
            ToolCall {
                unnumbered: 1,
                ..run14()
            },
            ToolCall {
                arguments: "{\"command\":\"$ASURA_SECRET_2\"}".into(),
                ..run14()
            },
            ToolCall {
                arguments: "{\"command\":\"$ASURA_SECRET_01\"}".into(),
                ..run14()
            },
            // A run exec_cmd has at most 16 variables.
            ToolCall {
                arguments: format!("{{\"command\":\"{}\"}}", tokens(17)),
                number_kinds: vec![SecretKind::Jwt; 17],
                ..run14()
            },
        ] {
            invalid14_record(p);
        }
        // Unknown codes in each code byte, by offset in run14's bytes.
        let base = raw14(&run14());
        let name = 33 + 8;
        let at = |field: usize| name + 4 + field;
        for (offset, value) in [
            (28, 0),
            (28, 3),
            (at(0), 11),
            (at(1), 0),
            (at(1), 9),
            (at(2), 16),
            (at(3), 5),
            (at(4), 0),
            (at(4), 5),
            (at(5), 4),
        ] {
            let mut bytes = base.clone();
            bytes[offset] = value;
            assert_eq!(
                decode_record(14, &bytes),
                Err(ReplayError::Corrupt),
                "offset {offset} value {value}"
            );
        }
        // Form 4 needs a reason; forms 1-3 take none.
        let mut bytes = base.clone();
        bytes[at(5)] = 1;
        assert_eq!(decode_record(14, &bytes), Err(ReplayError::Corrupt));
        // Bad number kind, flag, refusal and trailing bytes.
        let kinds = 59 + run14().arguments.len() + 2;
        for (offset, value) in [
            (kinds, 0),
            (kinds, 34),
            (base.len() - 5, 2),
            (base.len() - 5, 4),
            (base.len() - 5, 255),
        ] {
            let mut bytes = base.clone();
            bytes[offset] = value;
            assert_eq!(decode_record(14, &bytes), Err(ReplayError::Corrupt));
        }
        let mut bytes = base.clone();
        bytes.push(0);
        assert_eq!(decode_record(14, &bytes), Err(ReplayError::Corrupt));
        bytes.truncate(base.len() - 1);
        assert_eq!(decode_record(14, &bytes), Err(ReplayError::Corrupt));
        // Over-long strings decode as Corrupt.
        let mut over = base.clone();
        over[29..33].copy_from_slice(&513u32.to_be_bytes());
        assert_eq!(decode_record(14, &over), Err(ReplayError::Corrupt));
        let mut over = base;
        over[55..59].copy_from_slice(&64_001u32.to_be_bytes());
        assert_eq!(decode_record(14, &over), Err(ReplayError::Corrupt));
    }
    /// TCR-U02: retired kinds are UnsupportedFormat; the old kind 14 layout is
    /// Corrupt.
    #[test]
    fn tcr_u02_retired_kinds_and_old_layout_are_rejected() {
        for kind in [17, 22] {
            assert_eq!(
                decode_record(kind, &[]),
                Err(ReplayError::UnsupportedFormat)
            );
            assert_eq!(
                decode_record(kind, &[1; 200]),
                Err(ReplayError::UnsupportedFormat)
            );
        }
        // The retired ToolIntent layout: operation, generation, ordinal, tool
        // kind, path, offset and limit.
        for (kind, path, offset, limit) in [
            (1u8, "src/main.rs", 0u64, 4096u32),
            (2, ".", 0, 0),
            (3, "", 0, 0),
            (13, "", 0, 0),
            (16, "printf hello", 1000, 30),
        ] {
            let mut w = Writer(Vec::new());
            w.bytes(&[1; 16]);
            w.u64(1);
            w.u32(1);
            w.byte(kind);
            w.string(path, 1024).unwrap();
            w.u64(offset);
            w.u32(limit);
            assert_eq!(
                decode_record(14, &w.0),
                Err(ReplayError::Corrupt),
                "old kind 14 tool kind {kind}"
            );
        }
    }
    /// TCR-U03: the largest legal record is a 65,050-byte frame; one more
    /// argument byte is refused by the encoder.
    #[test]
    fn tcr_u03_largest_kind_14_fits_one_frame() {
        let prefix = format!("{{\"body\":\"{}", tokens(256));
        let fill = MAX_TOOL_ARGUMENTS - prefix.len() - 2;
        let text = format!("{prefix}{}\"}}", "a".repeat(fill));
        assert_eq!(text.len(), MAX_TOOL_ARGUMENTS);
        let largest = ToolCall {
            name: "n".repeat(MAX_STORED_TOOL_NAME),
            name_bytes: MAX_TOOL_NAME as u32,
            tool_operation: None,
            decision: ToolDecision::UnknownTool,
            fault: Some(ToolFault::UnregisteredName),
            form: ArgumentForm::Verbatim,
            arguments_bytes: MAX_TOOL_ARGUMENTS as u32,
            arguments: text.clone(),
            number_kinds: vec![SecretKind::Jwt; MAX_TOOL_NUMBERS],
            ..run14()
        };
        let bytes = round14(largest.clone());
        assert_eq!(bytes.len(), MAX_TOOL_CALL_PAYLOAD);
        assert_eq!(bytes.len(), 64_866);
        let frame = encode_frame(&context(), &Record::ToolCall(Box::new(largest.clone()))).unwrap();
        assert_eq!(frame.len(), 65_050);
        assert!(frame.len() <= MAX_FRAME);
        let mut over = largest;
        over.arguments.insert(9, 'a');
        assert_eq!(
            encode_frame(&context(), &Record::ToolCall(Box::new(over))),
            Err(ReplayError::Limit)
        );
    }
    fn context() -> FrameContext {
        FrameContext {
            installation_id: [9; 16],
            transition_id: [8; 16],
            sequence: 2,
            prior_digest: [7; 32],
            expected_revision: 1,
            owner_generation: 1,
        }
    }

    fn stream() -> CaptureStream {
        CaptureStream {
            observed_bytes: 10,
            stored_bytes: 12,
            digest: Some([6; 32]),
            replacements: 1,
            unnumbered_replacements: 0,
            literal_replacements: 1,
            kinds: vec![(SecretKind::GithubToken, 1)],
            loss_span_count: 0,
            withheld_bytes: 0,
            spans: vec![],
            degraded: false,
            discarded_bytes: 0,
            eof: true,
        }
    }
    /// The capture facts of the retired kind 23 (J3-E9), kept as a test
    /// builder: kind 28 applies the same capture rules, so each case below
    /// is encoded and checked as a kind 28 through [`as28`].
    #[derive(Clone, Debug)]
    struct CaptureSettled {
        state: CaptureState,
        reason: CaptureReason,
        exit_kind: u8,
        exit_value: u32,
        stdout: CaptureStream,
        stderr: CaptureStream,
        distinct_numbers: u32,
        numbers: Vec<CaptureNumber>,
        pause_count: u32,
        first_pause_ms: u64,
        paused_ms: u64,
        final_reservation: u64,
    }
    /// The kind 28 with these capture facts: `not_started` (reason
    /// `spawn_failed`) for capture state 4; otherwise `exited`, or
    /// `signalled` for exit kind 2, with cleanup confirmed.
    fn as28(p: &CaptureSettled) -> JobSettled {
        let started = p.state != CaptureState::NotStarted;
        JobSettled {
            job: [3; 16],
            outcome: match (started, p.exit_kind) {
                (false, _) => JobOutcome::NotStarted,
                (true, 2) => JobOutcome::Signalled,
                (true, _) => JobOutcome::Exited,
            },
            reason: if started {
                JobReason::None
            } else {
                JobReason::SpawnFailed
            },
            exit_kind: p.exit_kind,
            exit_value: p.exit_value,
            cleanup: if started {
                JobCleanup::Confirmed
            } else {
                JobCleanup::None
            },
            unconfirmed_reason: 0,
            escapees: 0,
            elapsed_ms: if started { 5 } else { 0 },
            capture_state: match p.state {
                CaptureState::Complete => JobCaptureState::Complete,
                CaptureState::Incomplete => JobCaptureState::Incomplete,
                CaptureState::Unconfirmed => JobCaptureState::Unconfirmed,
                CaptureState::NotStarted => JobCaptureState::NotStarted,
            },
            capture_reason: p.reason,
            stdout: JobStream {
                capture: p.stdout.clone(),
                lines: 0,
            },
            stderr: JobStream {
                capture: p.stderr.clone(),
                lines: 0,
            },
            distinct_numbers: p.distinct_numbers,
            numbers: p.numbers.clone(),
            pause_count: p.pause_count,
            first_pause_ms: p.first_pause_ms,
            paused_ms: p.paused_ms,
            final_reservation: p.final_reservation,
            settled_time: 1_760_000_000_002,
        }
    }
    fn settled() -> CaptureSettled {
        CaptureSettled {
            state: CaptureState::Complete,
            reason: CaptureReason::None,
            exit_kind: 1,
            exit_value: 0,
            stdout: stream(),
            stderr: CaptureStream {
                kinds: vec![],
                replacements: 0,
                literal_replacements: 0,
                ..stream()
            },
            distinct_numbers: 1,
            numbers: vec![CaptureNumber {
                number: 1,
                kind: SecretKind::GithubToken,
                origin: NumberOrigin::Command,
            }],
            pause_count: 0,
            first_pause_ms: 0,
            paused_ms: 0,
            final_reservation: INITIAL_RESERVATION,
        }
    }
    fn raw23(p: &CaptureSettled) -> Vec<u8> {
        let mut w = Writer(Vec::new());
        job_settled_fields(&mut w, &as28(p));
        w.0
    }
    fn round23(p: CaptureSettled) -> Vec<u8> {
        let record = Record::JobSettled(Box::new(as28(&p)));
        let bytes = payload(&record).unwrap();
        assert_eq!(decode_record(28, &bytes).unwrap(), record);
        bytes
    }
    fn invalid23(p: CaptureSettled) {
        let record = Record::JobSettled(Box::new(as28(&p)));
        assert_eq!(payload(&record), Err(ReplayError::Corrupt), "{p:?}");
        assert_eq!(
            decode_record(28, &raw23(&p)),
            Err(ReplayError::Corrupt),
            "{p:?}"
        );
    }
    /// Kind 28 offsets: capture state 37, capture reason 38, redaction 39,
    /// stdout block 40.
    const CAPTURE_AT: usize = 37;
    const STDOUT_AT: usize = 40;
    #[test]
    fn j3t_03_jr17_kind_23_is_unsupported() {
        // Kind 23 is retired with the J1 path: its bytes, or any payload, are
        // UnsupportedFormat like the other retired kinds.
        assert_eq!(
            decode_record(23, &raw23(&settled())),
            Err(ReplayError::UnsupportedFormat)
        );
        assert_eq!(decode_record(23, &[]), Err(ReplayError::UnsupportedFormat));
    }
    fn full_stream() -> CaptureStream {
        CaptureStream {
            observed_bytes: u64::MAX,
            stored_bytes: u64::MAX,
            digest: Some([6; 32]),
            replacements: u64::MAX,
            unnumbered_replacements: u64::MAX,
            literal_replacements: u64::MAX,
            kinds: SecretKind::ALL.iter().map(|k| (*k, u64::MAX)).collect(),
            loss_span_count: u64::MAX,
            withheld_bytes: u64::MAX,
            spans: (0..64)
                .map(|n| LossSpan {
                    stored_offset: if n == 63 { u64::MAX } else { n },
                    withheld_bytes: u64::MAX,
                    reason: [
                        LossReason::LineOverLimit,
                        LossReason::KeyBlockOverLimit,
                        LossReason::DetectorError,
                    ][n as usize % 3],
                })
                .collect(),
            degraded: true,
            discarded_bytes: 0,
            eof: true,
        }
    }
    #[test]
    fn j2t_01_kind_28_capture_fields_round_trip_every_bound_and_maximum_size() {
        let origins = [NumberOrigin::Command, NumberOrigin::Output];
        let max = CaptureSettled {
            exit_kind: 2,
            exit_value: 127,
            stdout: full_stream(),
            stderr: full_stream(),
            distinct_numbers: 16_384,
            numbers: (0..256u16)
                .map(|n| CaptureNumber {
                    number: if n == 255 { 16_384 } else { n + 1 },
                    kind: SecretKind::ALL[n as usize % 33],
                    origin: origins[n as usize % 2],
                })
                .collect(),
            pause_count: u32::MAX,
            first_pause_ms: u64::MAX,
            paused_ms: u64::MAX,
            final_reservation: u64::MAX - (u64::MAX % MIB),
            ..settled()
        };
        let bytes = round23(max.clone());
        assert_eq!(bytes.len(), 4_094);
        assert_eq!(bytes.len(), MAX_JOB_SETTLED_PAYLOAD);
        let frame = encode_frame(&context(), &Record::JobSettled(Box::new(as28(&max)))).unwrap();
        assert_eq!(frame.len(), 4_278);
        // Minimum: no kinds, spans or numbers; 40 + 2 × (101 + 8) + 6 + 20
        // + 8 + 8.
        let min = CaptureSettled {
            stdout: CaptureStream {
                kinds: vec![],
                replacements: 0,
                literal_replacements: 0,
                ..stream()
            },
            distinct_numbers: 0,
            numbers: vec![],
            final_reservation: 0,
            ..settled()
        };
        assert_eq!(round23(min).len(), 300);
        let without_digests = |s: CaptureStream| CaptureStream { digest: None, ..s };
        for (state, reason) in [
            (CaptureState::Complete, CaptureReason::None),
            (CaptureState::Incomplete, CaptureReason::StorageFault),
            (CaptureState::Incomplete, CaptureReason::Quota),
            (CaptureState::Incomplete, CaptureReason::Enospc),
            (CaptureState::Incomplete, CaptureReason::DetectorFault),
            (
                CaptureState::Incomplete,
                CaptureReason::DiscardedAfterCancel,
            ),
            (
                CaptureState::Unconfirmed,
                CaptureReason::SettlementUnconfirmed,
            ),
            (CaptureState::NotStarted, CaptureReason::None),
            (CaptureState::NotStarted, CaptureReason::StorageFault),
        ] {
            let mut p = CaptureSettled {
                state,
                reason,
                ..settled()
            };
            if matches!(state, CaptureState::Unconfirmed | CaptureState::NotStarted) {
                p.stdout = without_digests(p.stdout);
                p.stderr = without_digests(p.stderr);
            }
            if state == CaptureState::NotStarted {
                p.exit_kind = 0;
                for s in [&mut p.stdout, &mut p.stderr] {
                    s.observed_bytes = 0;
                    s.stored_bytes = 0;
                    s.eof = false;
                }
            }
            if state == CaptureState::Incomplete {
                // Incomplete allows missing EOF and discarded bytes.
                p.stdout.eof = false;
                p.stderr.discarded_bytes = u64::MAX;
            }
            let bytes = round23(p);
            assert_eq!(bytes[CAPTURE_AT], state as u8);
            assert_eq!(bytes[CAPTURE_AT + 1], reason as u8);
        }
        for (exit_kind, exit_value) in [(1, 0), (1, 255), (2, 1), (2, 127)] {
            round23(CaptureSettled {
                exit_kind,
                exit_value,
                ..settled()
            });
        }
        // Kind 28 has no unknown exit for a process that ran.
        invalid23(CaptureSettled {
            exit_kind: 0,
            exit_value: 0,
            ..settled()
        });
        for spans in [0, 1, 63, 64, 65, 1000] {
            let count = spans.min(64) as usize;
            round23(CaptureSettled {
                stdout: CaptureStream {
                    loss_span_count: spans,
                    spans: full_stream().spans[..count]
                        .iter()
                        .map(|s| LossSpan {
                            stored_offset: 12,
                            ..*s
                        })
                        .collect(),
                    ..stream()
                },
                ..settled()
            });
        }
        for kind in SecretKind::ALL {
            round23(CaptureSettled {
                numbers: vec![CaptureNumber {
                    number: 16_384,
                    kind,
                    origin: NumberOrigin::Output,
                }],
                distinct_numbers: 16_384,
                ..settled()
            });
        }
        let digest = round23(settled());
        // The stdout digest field is the SHA-256 of stored bytes, present flag 1.
        assert_eq!(digest[STDOUT_AT + 16], 1);
        assert_eq!(&digest[STDOUT_AT + 17..STDOUT_AT + 49], &[6; 32]);
        assert_eq!(digest[STDOUT_AT - 1], 1, "redaction ran");
    }
    #[test]
    fn j2t_01_kind_28_capture_fields_reject_each_invalid_value() {
        let s = settled;
        let none = |p: CaptureSettled| CaptureSettled {
            stdout: CaptureStream {
                digest: None,
                ..p.stdout
            },
            stderr: CaptureStream {
                digest: None,
                ..p.stderr
            },
            ..p
        };
        let not_started = || CaptureSettled {
            state: CaptureState::NotStarted,
            exit_kind: 0,
            stdout: CaptureStream {
                observed_bytes: 0,
                stored_bytes: 0,
                digest: None,
                ..stream()
            },
            stderr: CaptureStream {
                observed_bytes: 0,
                stored_bytes: 0,
                digest: None,
                ..s().stderr
            },
            ..s()
        };
        round23(not_started());
        let numbers = |list: &[u16]| {
            list.iter()
                .map(|n| CaptureNumber {
                    number: *n,
                    kind: SecretKind::Jwt,
                    origin: NumberOrigin::Output,
                })
                .collect::<Vec<_>>()
        };
        let span = |stored_offset| LossSpan {
            stored_offset,
            withheld_bytes: 1,
            reason: LossReason::DetectorError,
        };
        for p in [
            CaptureSettled {
                reason: CaptureReason::StorageFault,
                ..s()
            },
            CaptureSettled {
                stdout: CaptureStream {
                    digest: None,
                    ..stream()
                },
                ..s()
            },
            CaptureSettled {
                stderr: CaptureStream {
                    digest: None,
                    ..s().stderr
                },
                ..s()
            },
            CaptureSettled {
                stdout: CaptureStream {
                    eof: false,
                    ..stream()
                },
                ..s()
            },
            CaptureSettled {
                stderr: CaptureStream {
                    eof: false,
                    ..s().stderr
                },
                ..s()
            },
            CaptureSettled {
                stdout: CaptureStream {
                    discarded_bytes: 1,
                    ..stream()
                },
                ..s()
            },
            CaptureSettled {
                state: CaptureState::Incomplete,
                reason: CaptureReason::None,
                ..s()
            },
            CaptureSettled {
                state: CaptureState::Incomplete,
                reason: CaptureReason::SettlementUnconfirmed,
                ..s()
            },
            none(CaptureSettled {
                state: CaptureState::Incomplete,
                reason: CaptureReason::Quota,
                ..s()
            }),
            CaptureSettled {
                state: CaptureState::Incomplete,
                reason: CaptureReason::Quota,
                stderr: CaptureStream {
                    digest: None,
                    ..s().stderr
                },
                ..s()
            },
            none(CaptureSettled {
                state: CaptureState::Unconfirmed,
                reason: CaptureReason::DiscardedAfterCancel,
                ..s()
            }),
            none(CaptureSettled {
                state: CaptureState::Unconfirmed,
                reason: CaptureReason::None,
                ..s()
            }),
            CaptureSettled {
                state: CaptureState::Unconfirmed,
                reason: CaptureReason::SettlementUnconfirmed,
                stdout: CaptureStream {
                    digest: None,
                    ..stream()
                },
                ..s()
            },
            CaptureSettled {
                reason: CaptureReason::Quota,
                ..not_started()
            },
            CaptureSettled {
                reason: CaptureReason::SettlementUnconfirmed,
                ..not_started()
            },
            CaptureSettled {
                exit_kind: 1,
                ..not_started()
            },
            CaptureSettled {
                stdout: CaptureStream {
                    observed_bytes: 1,
                    ..not_started().stdout
                },
                ..not_started()
            },
            CaptureSettled {
                stderr: CaptureStream {
                    stored_bytes: 1,
                    ..not_started().stderr
                },
                ..not_started()
            },
            CaptureSettled {
                stdout: CaptureStream {
                    digest: Some([6; 32]),
                    ..not_started().stdout
                },
                ..not_started()
            },
            CaptureSettled {
                exit_kind: 0,
                exit_value: 1,
                ..s()
            },
            CaptureSettled {
                exit_kind: 1,
                exit_value: 256,
                ..s()
            },
            CaptureSettled {
                exit_kind: 2,
                exit_value: 0,
                ..s()
            },
            CaptureSettled {
                exit_kind: 2,
                exit_value: 128,
                ..s()
            },
            CaptureSettled {
                exit_kind: 3,
                exit_value: 0,
                ..s()
            },
            CaptureSettled {
                stdout: CaptureStream {
                    unnumbered_replacements: 2,
                    ..stream()
                },
                ..s()
            },
            CaptureSettled {
                stdout: CaptureStream {
                    literal_replacements: 2,
                    ..stream()
                },
                ..s()
            },
            CaptureSettled {
                stdout: CaptureStream {
                    kinds: vec![(SecretKind::Jwt, 0)],
                    ..stream()
                },
                ..s()
            },
            CaptureSettled {
                stdout: CaptureStream {
                    kinds: vec![(SecretKind::Jwt, 1), (SecretKind::Jwt, 1)],
                    ..stream()
                },
                ..s()
            },
            CaptureSettled {
                stdout: CaptureStream {
                    kinds: vec![(SecretKind::Jwt, 1), (SecretKind::PrivateKey, 1)],
                    ..stream()
                },
                ..s()
            },
            CaptureSettled {
                stdout: CaptureStream {
                    loss_span_count: 2,
                    spans: vec![span(0)],
                    ..stream()
                },
                ..s()
            },
            CaptureSettled {
                stdout: CaptureStream {
                    loss_span_count: 0,
                    spans: vec![span(0)],
                    ..stream()
                },
                ..s()
            },
            CaptureSettled {
                stdout: CaptureStream {
                    loss_span_count: 65,
                    spans: vec![span(0); 63],
                    ..stream()
                },
                ..s()
            },
            CaptureSettled {
                stdout: CaptureStream {
                    loss_span_count: 1,
                    spans: vec![span(13)],
                    ..stream()
                },
                ..s()
            },
            CaptureSettled {
                distinct_numbers: 16_385,
                ..s()
            },
            CaptureSettled {
                distinct_numbers: 16_384,
                numbers: (1..=257).map(|n| numbers(&[n])[0]).collect(),
                ..s()
            },
            CaptureSettled {
                distinct_numbers: 1,
                numbers: numbers(&[1, 2]),
                ..s()
            },
            CaptureSettled {
                distinct_numbers: 0,
                ..s()
            },
            CaptureSettled {
                distinct_numbers: 2,
                numbers: numbers(&[0]),
                ..s()
            },
            CaptureSettled {
                distinct_numbers: 2,
                numbers: numbers(&[16_385]),
                ..s()
            },
            CaptureSettled {
                distinct_numbers: 2,
                numbers: numbers(&[2, 1]),
                ..s()
            },
            CaptureSettled {
                distinct_numbers: 2,
                numbers: numbers(&[1, 1]),
                ..s()
            },
            CaptureSettled {
                final_reservation: 1,
                ..s()
            },
            CaptureSettled {
                final_reservation: INITIAL_RESERVATION + 1,
                ..s()
            },
        ] {
            invalid23(p);
        }
        // Wire-only values in the default record. stdout starts at 40 and has
        // one kind entry (110 bytes and an 8-byte line count): kind count
        // +73, loss count +83, span count +99, degraded +100, discarded +101,
        // EOF +109. stderr starts at 158 with no entries: EOF +100. Numbers
        // follow at 267.
        let stdout = STDOUT_AT;
        let stderr = stdout + 110 + 8;
        let numbers_at = stderr + 101 + 8;
        let wire: [(usize, u8); 20] = [
            (CAPTURE_AT, 0),
            (CAPTURE_AT, 6),
            (CAPTURE_AT + 1, 8),
            (CAPTURE_AT + 2, 0),
            (CAPTURE_AT + 2, 2),
            (stdout + 16, 2),
            (stderr + 16, 0),
            (stdout + 73, 34),
            (stdout + 73, 2),
            (stdout + 74, 0),
            (stdout + 74, 34),
            (stdout + 99, 1),
            (stdout + 100, 2),
            (stdout + 109, 2),
            (stderr + 100, 2),
            (numbers_at + 5, 2),
            (numbers_at + 6 + 2, 0),
            (numbers_at + 6 + 2, 34),
            (numbers_at + 6 + 3, 0),
            (numbers_at + 6 + 3, 3),
        ];
        let base = raw23(&s());
        assert!(decode_record(28, &base).is_ok());
        for (offset, value) in wire {
            let mut bytes = base.clone();
            assert_ne!(bytes[offset], value, "{offset}");
            bytes[offset] = value;
            assert_eq!(
                decode_record(28, &bytes),
                Err(ReplayError::Corrupt),
                "{offset} {value}"
            );
        }
        // An absent digest must be zero; a span reason must be 1–3.
        let mut bytes = raw23(&none(CaptureSettled {
            state: CaptureState::Unconfirmed,
            reason: CaptureReason::SettlementUnconfirmed,
            ..s()
        }));
        assert!(decode_record(28, &bytes).is_ok());
        bytes[stdout + 17] = 1;
        assert_eq!(decode_record(28, &bytes), Err(ReplayError::Corrupt));
        let with_span = CaptureSettled {
            stdout: CaptureStream {
                loss_span_count: 1,
                spans: vec![span(0)],
                ..stream()
            },
            ..s()
        };
        for reason in [0, 4] {
            let mut bytes = raw23(&with_span);
            assert!(decode_record(28, &bytes).is_ok());
            // The first span starts after the span count, at stdout + 100.
            assert_eq!(bytes[stdout + 100 + 16], 3);
            bytes[stdout + 100 + 16] = 1;
            assert!(decode_record(28, &bytes).is_ok());
            bytes[stdout + 100 + 16] = reason;
            assert_eq!(decode_record(28, &bytes), Err(ReplayError::Corrupt));
        }
        let mut bytes = base.clone();
        bytes.push(0);
        assert_eq!(decode_record(28, &bytes), Err(ReplayError::Corrupt));
        assert_eq!(
            decode_record(28, &base[..base.len() - 1]),
            Err(ReplayError::Corrupt)
        );
    }
    const REFUSALS: [RefusalCode; 14] = [
        RefusalCode::PlaceholderNotAvailable,
        RefusalCode::UnparsedCommand,
        RefusalCode::SecretInHeredoc,
        RefusalCode::SecretInSubstitution,
        RefusalCode::SecretInInnerScript,
        RefusalCode::SecretSingleQuoted,
        RefusalCode::SecretAnsiQuoted,
        RefusalCode::SecretMixedQuoting,
        RefusalCode::SecretWithShellSyntax,
        RefusalCode::TooManySecrets,
        RefusalCode::ResidualSecret,
        RefusalCode::DetectionUnavailable,
        RefusalCode::RewrittenCommandTooLong,
        RefusalCode::SecretInCwd,
    ];
    #[test]
    fn j1t_01_code_tables_are_fixed() {
        let refusals = [
            "placeholder_not_available",
            "unparsed_command",
            "secret_in_heredoc",
            "secret_in_substitution",
            "secret_in_inner_script",
            "secret_single_quoted",
            "secret_ansi_quoted",
            "secret_mixed_quoting",
            "secret_with_shell_syntax",
            "too_many_secrets",
            "residual_secret",
            "secret_detection_unavailable",
            "rewritten_command_too_long",
            "secret_in_cwd",
        ];
        for (index, name) in refusals.iter().enumerate() {
            let code = index as u8 + 1;
            let variant = refusal_from(code).unwrap();
            assert_eq!(variant.as_str(), *name);
            assert_eq!(refusal_code(variant), code);
            assert_eq!(REFUSALS[index], variant);
            // The stored byte, through the real encoder (kind 14 field 21).
            // Detection unavailable never appears as a kind 14 refusal code.
            // A kept code is a refusal; every other code is the fallback
            // reason of a whole-command run (SL7).
            if variant != RefusalCode::DetectionUnavailable {
                let refusal = Some(ExecCmdRefusal {
                    code: variant,
                    kind: None,
                    word: None,
                });
                let (record, wrong) = if variant.kept() {
                    (refused14(), whole14())
                } else {
                    (whole14(), refused14())
                };
                let bytes = round14(ToolCall { refusal, ..record });
                assert_eq!(bytes[bytes.len() - 4], code);
                invalid14_record(ToolCall { refusal, ..wrong });
            }
        }
        for code in [0, 15, 255] {
            assert_eq!(refusal_from(code), Err(ReplayError::Corrupt));
        }
        let kinds = [
            "private-key",
            "aws-access-key",
            "aws-secret-key",
            "github-token",
            "gitlab-token",
            "pypi-token",
            "docker-token",
            "npm-token",
            "bitbucket-token",
            "vault-token",
            "fly-token",
            "webhook-secret",
            "mailgun-key",
            "google-oauth",
            "azure-key",
            "slack-token",
            "slack-webhook",
            "chat-webhook",
            "stripe-key",
            "anthropic-key",
            "openai-key",
            "google-api-key",
            "jwt",
            "url-password",
            "url-secret",
            "auth-header",
            "session-cookie",
            "password-hash",
            "assigned-secret",
            "env-secret",
            "cli-password",
            "netrc-password",
            "wifi-password",
        ];
        assert_eq!(kinds.len(), SecretKind::ALL.len());
        for (index, name) in kinds.iter().enumerate() {
            let code = index as u8 + 1;
            let kind = kind_from(code).unwrap();
            assert_eq!(kind.as_str(), *name);
            assert_eq!(kind_code(kind), code);
            // Codes are the rule-set 1 positions of `SecretKind::ALL`.
            assert_eq!(SecretKind::ALL[index], kind);
            let p = ToolCall {
                number_kinds: vec![kind],
                ..run14()
            };
            let at = 59 + p.arguments.len() + 2;
            let bytes = round14(p);
            assert_eq!(bytes[at], code);
        }
        for code in [0, 34, 255] {
            assert_eq!(kind_from(code), Err(ReplayError::Corrupt));
        }
        for (code, reason) in [
            (1, LossReason::LineOverLimit),
            (2, LossReason::KeyBlockOverLimit),
            (3, LossReason::DetectorError),
        ] {
            assert_eq!(loss_from(code).unwrap(), reason);
            assert_eq!(loss_code(reason), code);
        }
        for (code, origin) in [(1, NumberOrigin::Command), (2, NumberOrigin::Output)] {
            assert_eq!(origin_from(code).unwrap(), origin);
            assert_eq!(origin_code(origin), code);
        }
        for (code, state) in [
            (1, CaptureState::Complete),
            (2, CaptureState::Incomplete),
            (3, CaptureState::Unconfirmed),
            (4, CaptureState::NotStarted),
        ] {
            assert_eq!(state as u8, code);
        }
        assert_eq!(CaptureReason::SettlementUnconfirmed as u8, 6);
        // Kind 14 code tables (tool-call-records.md).
        let operations = [
            "read_file",
            "list_directory",
            "status",
            "tools",
            "audit",
            "list_notes",
            "get_note",
            "note_sources",
            "create_note",
            "exec_cmd",
            "list",
            "status",
            "read",
            "wait",
            "cancel",
            "note",
        ];
        for (index, name) in operations.iter().enumerate() {
            let op = ToolOperation::from_code(index as u8 + 1).unwrap();
            assert_eq!((op as u8, op.as_str()), (index as u8 + 1, *name));
            // JR11: codes 10 to 16 resolve with kind 29.
            assert_eq!(op.job_result(), index >= 9);
            assert_eq!(
                op.tool() == RegisteredTool::Job,
                index >= 10,
                "{name} belongs to the job tool"
            );
        }
        assert_eq!(ToolOperation::from_code(0), None);
        assert_eq!(ToolOperation::from_code(17), None);
        let decisions = [
            ("run", None),
            ("invalid_arguments", Some(3)),
            ("unknown_tool", Some(3)),
            ("refused", Some(3)),
            ("busy", Some(4)),
            ("denied", Some(2)),
            ("limit", Some(7)),
            ("cancelled", Some(6)),
        ];
        for (index, (name, status)) in decisions.iter().enumerate() {
            let d = ToolDecision::from_code(index as u8 + 1).unwrap();
            assert_eq!(
                (d as u8, d.as_str(), d.status()),
                (index as u8 + 1, *name, *status)
            );
        }
        assert_eq!(ToolDecision::from_code(9), None);
        let faults = [
            "invalid_json",
            "json_limit",
            "not_object",
            "duplicate_key",
            "unknown_command",
            "missing_field",
            "unexpected_field",
            "wrong_type",
            "out_of_range",
            "unsafe_path",
            "invalid_version_id",
            "arguments_over_limit",
            "name_over_limit",
            "invalid_text",
            "unregistered_name",
            "invalid_job_id",
        ];
        for (index, name) in faults.iter().enumerate() {
            let f = ToolFault::from_code(index as u8 + 1).unwrap();
            assert_eq!((f as u8, f.as_str()), (index as u8 + 1, *name));
        }
        assert_eq!(ToolFault::from_code(17), None);
        // Field 10 (J3-E3): the codes per decision; the J1 busy codes 4 and
        // the reasons of other decisions are retired.
        for (decision, code, reason) in [
            (ToolDecision::Busy, 1, "queue_full"),
            (ToolDecision::Busy, 2, "jobs_full"),
            (ToolDecision::Busy, 3, "closed"),
            (ToolDecision::Denied, 1, "execution_off"),
        ] {
            let r = ToolDecisionReason::from_code(decision, code)
                .unwrap()
                .unwrap();
            assert_eq!(
                (r.code(), r.as_str(), r.decision()),
                (code, reason, decision)
            );
        }
        for decision in ToolDecision::ALL {
            assert_eq!(ToolDecisionReason::from_code(decision, 0), Some(None));
        }
        for (decision, code) in [
            (ToolDecision::Busy, 4),
            (ToolDecision::Busy, 0xff),
            (ToolDecision::Denied, 2),
            (ToolDecision::Run, 1),
            (ToolDecision::Refused, 1),
            (ToolDecision::Cancelled, 1),
        ] {
            assert_eq!(ToolDecisionReason::from_code(decision, code), None);
        }
        for (code, reason) in [
            (1, NotRetainedReason::Oversized),
            (2, NotRetainedReason::DetectionUnavailable),
            (3, NotRetainedReason::UnverifiableEscape),
        ] {
            assert_eq!(reason as u8, code);
            assert_eq!(
                form_from(4, code).unwrap(),
                ArgumentForm::NotRetained(reason)
            );
        }
        assert_eq!(ToolCallSource::GeneratedContent as u8, 1);
        assert_eq!(ToolCallSource::ProviderJson as u8, 2);
        assert_eq!(
            TOOL_NAMES,
            ["project", "memory", "service", "exec_cmd", "job"]
        );
        assert_eq!(RegisteredTool::lookup("job"), Some(RegisteredTool::Job));
    }
    #[test]
    fn j1t_01_records_hold_no_value_or_value_digest() {
        // Kind 14 holds the rewritten command inside its only variable-length
        // argument field; the number kinds are kind codes only and the only
        // 32-byte field is the rule-set digest.
        for count in [1, 16] {
            let text = format!(
                "{{\"command\":\"{}\"}}",
                (1..=count)
                    .map(|n| format!("\\\"$ASURA_SECRET_{n}\\\""))
                    .collect::<String>()
            );
            let p = ToolCall {
                arguments: text.clone(),
                arguments_bytes: 64,
                number_kinds: vec![SecretKind::AuthHeader; count],
                ..run14()
            };
            let bytes = round14(p);
            assert_eq!(bytes.len(), 98 + 8 + text.len() + count);
            assert_eq!(&bytes[59..59 + text.len()], text.as_bytes());
            let kinds = 59 + text.len() + 2;
            assert!(bytes[kinds..kinds + count].iter().all(|k| *k == 26));
            let digest = kinds + count + 8;
            assert_eq!(&bytes[digest..digest + 32], &[5; 32]);
        }
        // Kind 28 has no string field: its size depends only on entry counts.
        let bytes = round23(settled());
        assert_eq!(
            bytes.len(),
            40 + (101 + 9 + 8) + (101 + 8) + 6 + 4 + 20 + 8 + 8
        );
    }

    // J2T-01 (unit): kinds 26, 27 and 28 (tool-job-controller.md, J2 records).
    fn intent26() -> JobIntent {
        JobIntent {
            origin: JobOrigin::Model {
                operation: [1; 16],
                generation: 1,
                ordinal: 1,
            },
            agent: 0,
            time: 1_760_000_000_000,
        }
    }
    fn cancel27(reason: JobReason) -> JobCancelRequested {
        JobCancelRequested {
            job: [3; 16],
            requester: reason.requester().unwrap(),
            reason,
            kill: reason == JobReason::PersonKill,
        }
    }
    fn job_stream(capture: CaptureStream) -> JobStream {
        JobStream {
            lines: capture.stored_bytes.min(3),
            capture,
        }
    }
    /// An `exited` settlement with cleanup confirmed and complete capture.
    fn settled28() -> JobSettled {
        let base = settled();
        JobSettled {
            job: [3; 16],
            outcome: JobOutcome::Exited,
            reason: JobReason::None,
            exit_kind: 1,
            exit_value: 0,
            cleanup: JobCleanup::Confirmed,
            unconfirmed_reason: 0,
            escapees: 0,
            elapsed_ms: 5,
            capture_state: JobCaptureState::Complete,
            capture_reason: CaptureReason::None,
            stdout: job_stream(base.stdout),
            stderr: job_stream(base.stderr),
            distinct_numbers: base.distinct_numbers,
            numbers: base.numbers,
            pause_count: 0,
            first_pause_ms: 0,
            paused_ms: 0,
            final_reservation: INITIAL_RESERVATION,
            settled_time: 1_760_000_000_002,
        }
    }
    /// A consistent settlement for each outcome and an allowed reason.
    fn job28(outcome: JobOutcome, reason: JobReason) -> JobSettled {
        let unstarted = |s: JobStream| JobStream {
            capture: CaptureStream {
                observed_bytes: 0,
                stored_bytes: 0,
                digest: None,
                eof: false,
                ..s.capture
            },
            lines: 0,
        };
        match outcome {
            JobOutcome::Exited => JobSettled {
                reason,
                ..settled28()
            },
            JobOutcome::Signalled => JobSettled {
                outcome,
                reason,
                exit_kind: 2,
                exit_value: 15,
                ..settled28()
            },
            JobOutcome::Cancelled => JobSettled {
                outcome,
                reason,
                exit_kind: 2,
                exit_value: 9,
                capture_state: JobCaptureState::Incomplete,
                capture_reason: CaptureReason::DiscardedAfterCancel,
                cleanup: JobCleanup::Late,
                unconfirmed_reason: 9,
                escapees: 3,
                ..settled28()
            },
            JobOutcome::NotStarted => {
                let base = settled28();
                JobSettled {
                    outcome,
                    reason,
                    exit_kind: 0,
                    exit_value: 0,
                    cleanup: JobCleanup::None,
                    escapees: 0,
                    elapsed_ms: 0,
                    capture_state: JobCaptureState::NotStarted,
                    stdout: unstarted(base.stdout.clone()),
                    stderr: unstarted(base.stderr.clone()),
                    final_reservation: 0,
                    ..base
                }
            }
            JobOutcome::Interrupted => JobSettled {
                reason,
                ..JobSettled::interrupted([3; 16], 1_760_000_000_001)
            },
        }
    }
    fn raw(record: &Record) -> Vec<u8> {
        let mut w = Writer(Vec::new());
        match record {
            Record::JobIntent(p) => job_intent_fields(&mut w, p),
            Record::JobCancelRequested(p) => job_cancel_fields(&mut w, p),
            Record::JobSettled(p) => job_settled_fields(&mut w, p),
            Record::JobResult(p) => job_result_fields(&mut w, p),
            Record::JobRetention(p) => job_retention_fields(&mut w, p),
            _ => unreachable!(),
        }
        w.0
    }
    fn round_job(record: Record) -> Vec<u8> {
        let bytes = payload(&record).unwrap();
        assert_eq!(bytes, raw(&record));
        assert_eq!(decode_record(record.kind(), &bytes).unwrap(), record);
        bytes
    }
    fn invalid_job(record: Record) {
        assert_eq!(payload(&record), Err(ReplayError::Corrupt), "{record:?}");
        assert_eq!(
            decode_record(record.kind(), &raw(&record)),
            Err(ReplayError::Corrupt),
            "{record:?}"
        );
    }
    fn r26(p: JobIntent) -> Record {
        Record::JobIntent(p)
    }
    fn r28(p: JobSettled) -> Record {
        Record::JobSettled(Box::new(p))
    }
    #[test]
    fn j2t_01_kinds_26_to_28_round_trip_every_bound_and_maximum_size() {
        // Kind 26: model and person layouts; every reference bound.
        let model = round_job(r26(intent26()));
        assert_eq!(model.len(), MAX_JOB_INTENT_PAYLOAD);
        assert_eq!((model[0], &model[1..5]), (1, &[0u8; 4][..]));
        for (generation, ordinal) in [(1, 1), (u64::MAX, MAX_RECORDED_CALLS)] {
            round_job(r26(JobIntent {
                origin: JobOrigin::Model {
                    operation: [0xff; 16],
                    generation,
                    ordinal,
                },
                agent: 0,
                time: 1_760_000_000_000,
            }));
        }
        // Field 4 and kind 28 field 22: any wall time, 0 when the clock read
        // failed; replay never compares them (J4-E1).
        for time in [0, 1, u64::MAX] {
            let bytes = round_job(r26(JobIntent { time, ..intent26() }));
            assert_eq!(&bytes[33..], &time.to_be_bytes());
            let bytes = round_job(r28(JobSettled {
                settled_time: time,
                ..settled28()
            }));
            assert_eq!(&bytes[bytes.len() - 8..], &time.to_be_bytes());
            round_job(r28(JobSettled::interrupted([3; 16], time)));
        }
        let frame = encode_frame(&context(), &r26(intent26())).unwrap();
        assert_eq!(frame.len(), HEADER + TRAILER + 41);
        let person = round_job(r26(JobIntent {
            origin: JobOrigin::Person { input: [7; 16] },
            agent: 0,
            time: 1_760_000_000_000,
        }));
        assert_eq!(person.len(), 29);
        assert_eq!(person[0], 2);
        assert_eq!(&person[5..21], &[7; 16]);
        assert_eq!(&person[21..], &1_760_000_000_000u64.to_be_bytes());
        // Kind 27: every reason a cancel trigger may carry.
        for reason in JobReason::ALL
            .into_iter()
            .filter(|r| r.requester().is_some())
        {
            let bytes = round_job(Record::JobCancelRequested(cancel27(reason)));
            assert_eq!(bytes.len(), JOB_CANCEL_PAYLOAD);
            assert_eq!(bytes[17], reason as u8);
        }
        let frame = encode_frame(
            &context(),
            &Record::JobCancelRequested(cancel27(JobReason::PersonKill)),
        )
        .unwrap();
        assert_eq!(frame.len(), 203);
        // Kind 28 at its maximum: every stream entry, 256 numbers.
        let origins = [NumberOrigin::Command, NumberOrigin::Output];
        let max = JobSettled {
            outcome: JobOutcome::Signalled,
            reason: JobReason::Enospc,
            exit_kind: 2,
            exit_value: 127,
            cleanup: JobCleanup::Late,
            unconfirmed_reason: 13,
            escapees: u32::MAX,
            elapsed_ms: u64::MAX,
            capture_state: JobCaptureState::Incomplete,
            capture_reason: CaptureReason::Enospc,
            stdout: JobStream {
                capture: full_stream(),
                lines: u64::MAX,
            },
            stderr: JobStream {
                capture: full_stream(),
                lines: u64::MAX,
            },
            distinct_numbers: 16_384,
            numbers: (0..256u16)
                .map(|n| CaptureNumber {
                    number: if n == 255 { 16_384 } else { n + 1 },
                    kind: SecretKind::ALL[n as usize % 33],
                    origin: origins[n as usize % 2],
                })
                .collect(),
            pause_count: u32::MAX,
            first_pause_ms: u64::MAX,
            paused_ms: u64::MAX,
            final_reservation: u64::MAX - (u64::MAX % MIB),
            ..settled28()
        };
        let bytes = round_job(r28(max.clone()));
        assert_eq!(bytes.len(), MAX_JOB_SETTLED_PAYLOAD);
        assert_eq!(bytes.len(), 4_094);
        let frame = encode_frame(&context(), &r28(max)).unwrap();
        assert_eq!(frame.len(), 4_278);
        // Every outcome with every reason the table allows.
        let mut pairs = 0;
        for reason in JobReason::ALL {
            for outcome in JobOutcome::ALL.into_iter().filter(|o| reason.allows(*o)) {
                let bytes = round_job(r28(job28(outcome, reason)));
                assert_eq!((bytes[16], bytes[17]), (outcome as u8, reason as u8));
                pairs += 1;
            }
        }
        // Reasons 0 to 18, then 19 `execution_off` (ESCN) and 20
        // `outside_sandbox_off` (N).
        assert_eq!(pairs, 2 + 7 * 4 + 3 + 2 * 2 + 1 + 7 + 4 + 1);
        // Exit and cleanup bounds.
        for (exit_kind, exit_value) in [(1, 0), (1, 255)] {
            round_job(r28(JobSettled {
                exit_kind,
                exit_value,
                ..settled28()
            }));
        }
        for (exit_kind, exit_value) in [(2, 1), (2, 127)] {
            round_job(r28(JobSettled {
                exit_kind,
                exit_value,
                ..job28(JobOutcome::Signalled, JobReason::None)
            }));
        }
        for unconfirmed_reason in [9, 13] {
            round_job(r28(JobSettled {
                cleanup: JobCleanup::Late,
                unconfirmed_reason,
                ..settled28()
            }));
        }
        // Capture states 1-3 with a process; the kind 23 rules hold.
        for (state, reason) in [
            (JobCaptureState::Incomplete, CaptureReason::Quota),
            (JobCaptureState::Incomplete, CaptureReason::StorageFault),
            (
                JobCaptureState::Unconfirmed,
                CaptureReason::SettlementUnconfirmed,
            ),
        ] {
            let mut p = JobSettled {
                capture_state: state,
                capture_reason: reason,
                ..settled28()
            };
            if state == JobCaptureState::Unconfirmed {
                p.stdout.capture.digest = None;
                p.stderr.capture.digest = None;
            }
            round_job(r28(p));
        }
        // Not started with a storage fault (capture unavailable).
        round_job(r28(JobSettled {
            capture_reason: CaptureReason::StorageFault,
            ..job28(JobOutcome::NotStarted, JobReason::CaptureUnavailable)
        }));
        // Interrupted: redaction ran is 0 and every capture field is zero.
        let interrupted = round_job(r28(JobSettled::interrupted([3; 16], 1_760_000_000_001)));
        assert_eq!(interrupted[37..40], [5, 0, 0]);
        let time = interrupted.len() - 8;
        assert!(interrupted[40..time].iter().all(|b| *b == 0));
        assert_eq!(&interrupted[time..], &1_760_000_000_001u64.to_be_bytes());
        assert_eq!(round_job(r28(settled28()))[39], 1, "redaction ran");
    }
    #[test]
    fn j2t_01_kinds_26_to_28_reject_each_invalid_field() {
        // Kind 26.
        let model = |operation, generation, ordinal| JobIntent {
            origin: JobOrigin::Model {
                operation,
                generation,
                ordinal,
            },
            agent: 0,
            time: 1_760_000_000_000,
        };
        for p in [
            JobIntent {
                agent: 1,
                time: 1_760_000_000_000,
                ..intent26()
            },
            JobIntent {
                agent: u32::MAX,
                time: 1_760_000_000_000,
                ..intent26()
            },
            model([0; 16], 1, 1),
            model([1; 16], 0, 1),
            model([1; 16], 1, 0),
            model([1; 16], 1, MAX_RECORDED_CALLS + 1),
            JobIntent {
                origin: JobOrigin::Person { input: [0; 16] },
                agent: 0,
                time: 1_760_000_000_000,
            },
        ] {
            invalid_job(r26(p));
        }
        let mut bytes = raw(&r26(intent26()));
        for origin in [0, 3, 0xff] {
            bytes[0] = origin;
            assert_eq!(decode_record(26, &bytes), Err(ReplayError::Corrupt));
        }
        // A model reference in a person layout, or trailing bytes.
        let mut bytes = raw(&r26(intent26()));
        bytes[0] = 2;
        assert_eq!(decode_record(26, &bytes), Err(ReplayError::Corrupt));
        let mut bytes = raw(&r26(intent26()));
        bytes.push(0);
        assert_eq!(decode_record(26, &bytes), Err(ReplayError::Corrupt));
        bytes.truncate(32);
        assert_eq!(decode_record(26, &bytes), Err(ReplayError::Corrupt));
        // Kind 27: requester, reason and kill agree with the reason table.
        for reason in JobReason::ALL {
            for code in 1..=4 {
                let requester = JobRequester::from_code(code).unwrap();
                for kill in [false, true] {
                    let p = JobCancelRequested {
                        job: [3; 16],
                        requester,
                        reason,
                        kill,
                    };
                    let valid = reason.requester() == Some(requester)
                        && kill == (reason == JobReason::PersonKill);
                    let record = Record::JobCancelRequested(p);
                    if valid {
                        round_job(record);
                    } else {
                        invalid_job(record);
                    }
                }
            }
        }
        invalid_job(Record::JobCancelRequested(JobCancelRequested {
            job: [0; 16],
            ..cancel27(JobReason::ModelCancel)
        }));
        let good = raw(&Record::JobCancelRequested(cancel27(JobReason::Shutdown)));
        for (at, value) in [(16, 0), (16, 5), (17, 20), (17, 21), (17, 0xff), (18, 2)] {
            let mut bytes = good.clone();
            bytes[at] = value;
            assert_eq!(decode_record(27, &bytes), Err(ReplayError::Corrupt));
        }
        for len in [18, 20] {
            let mut bytes = good.clone();
            bytes.resize(len, 0);
            assert_eq!(decode_record(27, &bytes), Err(ReplayError::Corrupt));
        }
        // Kind 28: each reason with each outcome it does not allow.
        for reason in JobReason::ALL {
            for outcome in JobOutcome::ALL.into_iter().filter(|o| !reason.allows(*o)) {
                let allowed = JobOutcome::ALL
                    .into_iter()
                    .find(|o| reason.allows(*o))
                    .unwrap();
                let p = JobSettled {
                    reason,
                    ..job28(outcome, job_reason_for(outcome))
                };
                assert!(!reason.allows(outcome) && reason.allows(allowed));
                invalid_job(r28(p));
            }
        }
        let s = settled28;
        let signalled = || job28(JobOutcome::Signalled, JobReason::None);
        let cancelled = || job28(JobOutcome::Cancelled, JobReason::ModelCancel);
        let not_started = || job28(JobOutcome::NotStarted, JobReason::SpawnFailed);
        let interrupted = || JobSettled::interrupted([3; 16], 1_760_000_000_001);
        for p in [
            JobSettled {
                job: [0; 16],
                ..s()
            },
            // Exit kind and value per outcome.
            JobSettled {
                exit_kind: 2,
                exit_value: 15,
                ..s()
            },
            JobSettled {
                exit_kind: 0,
                ..s()
            },
            JobSettled {
                exit_value: 256,
                ..s()
            },
            JobSettled {
                exit_kind: 1,
                exit_value: 0,
                ..signalled()
            },
            JobSettled {
                exit_value: 0,
                ..signalled()
            },
            JobSettled {
                exit_value: 128,
                ..signalled()
            },
            JobSettled {
                exit_kind: 0,
                exit_value: 0,
                ..cancelled()
            },
            JobSettled {
                exit_kind: 3,
                ..cancelled()
            },
            JobSettled {
                exit_kind: 1,
                ..not_started()
            },
            JobSettled {
                exit_value: 1,
                ..not_started()
            },
            JobSettled {
                exit_kind: 2,
                exit_value: 9,
                ..interrupted()
            },
            // Cleanup, unconfirmed reason, escapees and elapsed.
            JobSettled {
                cleanup: JobCleanup::None,
                ..s()
            },
            JobSettled {
                cleanup: JobCleanup::None,
                ..cancelled()
            },
            JobSettled {
                unconfirmed_reason: 9,
                ..s()
            },
            JobSettled {
                unconfirmed_reason: 0,
                ..cancelled()
            },
            JobSettled {
                unconfirmed_reason: 4,
                ..cancelled()
            },
            JobSettled {
                cleanup: JobCleanup::Confirmed,
                ..not_started()
            },
            JobSettled {
                escapees: 1,
                ..not_started()
            },
            JobSettled {
                elapsed_ms: 1,
                ..not_started()
            },
            JobSettled {
                cleanup: JobCleanup::Late,
                unconfirmed_reason: 9,
                ..interrupted()
            },
            JobSettled {
                escapees: 1,
                ..interrupted()
            },
            JobSettled {
                elapsed_ms: 1,
                ..interrupted()
            },
            // Capture state per outcome.
            JobSettled {
                capture_state: JobCaptureState::NotStarted,
                ..s()
            },
            JobSettled {
                capture_state: JobCaptureState::Interrupted,
                ..s()
            },
            JobSettled {
                capture_state: JobCaptureState::Complete,
                ..not_started()
            },
            JobSettled {
                capture_state: JobCaptureState::Interrupted,
                ..not_started()
            },
            JobSettled {
                capture_state: JobCaptureState::NotStarted,
                ..interrupted()
            },
            // Kind 23 field rules for states 1-4.
            JobSettled {
                capture_reason: CaptureReason::Quota,
                ..s()
            },
            JobSettled {
                capture_reason: CaptureReason::None,
                ..cancelled()
            },
            JobSettled {
                capture_reason: CaptureReason::Quota,
                ..not_started()
            },
            JobSettled {
                final_reservation: 1,
                ..s()
            },
            JobSettled {
                distinct_numbers: 0,
                ..s()
            },
            // Line counts never exceed the stored bytes.
            JobSettled {
                stdout: JobStream {
                    lines: 13,
                    ..s().stdout
                },
                ..s()
            },
            JobSettled {
                stderr: JobStream {
                    lines: 1,
                    ..not_started().stderr
                },
                ..not_started()
            },
            // Interrupted: every capture field zero.
            JobSettled {
                capture_reason: CaptureReason::StorageFault,
                ..interrupted()
            },
            JobSettled {
                stdout: JobStream {
                    lines: 0,
                    ..s().stdout
                },
                ..interrupted()
            },
            JobSettled {
                stderr: JobStream {
                    capture: CaptureStream {
                        eof: true,
                        ..CaptureStream::default()
                    },
                    lines: 0,
                },
                ..interrupted()
            },
            JobSettled {
                distinct_numbers: 1,
                ..interrupted()
            },
            JobSettled {
                numbers: s().numbers,
                distinct_numbers: 1,
                ..interrupted()
            },
            JobSettled {
                pause_count: 1,
                ..interrupted()
            },
            JobSettled {
                first_pause_ms: 1,
                ..interrupted()
            },
            JobSettled {
                paused_ms: 1,
                ..interrupted()
            },
            JobSettled {
                final_reservation: MIB,
                ..interrupted()
            },
        ] {
            invalid_job(r28(p));
        }
        // Unknown codes and the redaction byte.
        let good = raw(&r28(settled28()));
        for (at, value) in [
            (16, 0),
            (16, 6),
            (17, 21),
            (23, 3),
            (37, 0),
            (37, 6),
            (38, 7),
            (39, 0),
            (39, 2),
        ] {
            let mut bytes = good.clone();
            bytes[at] = value;
            assert_eq!(decode_record(28, &bytes), Err(ReplayError::Corrupt), "{at}");
        }
        let mut bytes = raw(&r28(interrupted()));
        bytes[39] = 1;
        assert_eq!(decode_record(28, &bytes), Err(ReplayError::Corrupt));
        let mut bytes = good;
        bytes.push(0);
        assert_eq!(decode_record(28, &bytes), Err(ReplayError::Corrupt));
    }
    /// A reason that the table allows for `outcome`.
    fn job_reason_for(outcome: JobOutcome) -> JobReason {
        match outcome {
            JobOutcome::Exited | JobOutcome::Signalled => JobReason::None,
            JobOutcome::Cancelled => JobReason::ModelCancel,
            JobOutcome::NotStarted => JobReason::SetupFailed,
            JobOutcome::Interrupted => JobReason::Restart,
        }
    }
    #[test]
    fn j2t_01_job_code_tables_are_fixed() {
        let names = [
            "none",
            "model_cancel",
            "turn_ended",
            "client_cancel",
            "person_stop",
            "person_kill",
            "identity_changed",
            "shutdown",
            "capture_fault",
            "quota",
            "enospc",
            "restart",
            "cwd_unavailable",
            "project_changed",
            "sandbox_unavailable",
            "spawn_failed",
            "capture_unavailable",
            "protocol_fault",
            "setup_failed",
            "execution_off",
            "outside_sandbox_off",
        ];
        for (code, name) in names.iter().enumerate() {
            let reason = JobReason::from_code(code as u8).unwrap();
            assert_eq!((reason as u8, reason.as_str()), (code as u8, *name));
        }
        assert_eq!(JobReason::from_code(21), None);
        use JobRequester::*;
        let requesters = [
            None,
            Some(Model),
            Some(Model),
            Some(Client),
            Some(Person),
            Some(Person),
            Some(Service),
            Some(Service),
            Some(Service),
            Some(Service),
            Some(Service),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(Service),
            None,
        ];
        assert_eq!(requesters.len(), JobReason::ALL.len());
        for (reason, requester) in JobReason::ALL.into_iter().zip(requesters) {
            assert_eq!(reason.requester(), requester, "{reason:?}");
        }
        assert_eq!(
            [Model, Client, Person, Service].map(|r| r as u8),
            [1, 2, 3, 4]
        );
        assert_eq!(JobRequester::from_code(0), None);
        assert_eq!(JobRequester::from_code(5), None);
        // The outcome column of the reason table: E S C N I.
        let table: [&str; 21] = [
            "ES", "ESCN", "ESCN", "ESCN", "ESCN", "ESCN", "ESCN", "ESCN", "ESC", "ES", "ES", "I",
            "N", "N", "N", "N", "N", "N", "N", "ESCN", "N",
        ];
        for (reason, allowed) in JobReason::ALL.into_iter().zip(table) {
            for (outcome, letter) in JobOutcome::ALL.into_iter().zip(['E', 'S', 'C', 'N', 'I']) {
                assert_eq!(
                    reason.allows(outcome),
                    allowed.contains(letter),
                    "{reason:?} {outcome:?}"
                );
            }
        }
        for (code, name) in [
            "exited",
            "signalled",
            "cancelled",
            "not_started",
            "interrupted",
        ]
        .iter()
        .enumerate()
        {
            let outcome = JobOutcome::from_code(code as u8 + 1).unwrap();
            assert_eq!((outcome as u8, outcome.as_str()), (code as u8 + 1, *name));
        }
        assert_eq!(JobOutcome::from_code(0), None);
        assert_eq!(JobOutcome::from_code(6), None);
        assert_eq!(
            [JobCleanup::None, JobCleanup::Confirmed, JobCleanup::Late].map(|c| c as u8),
            [0, 1, 2]
        );
        assert_eq!(
            [
                JobCaptureState::Complete,
                JobCaptureState::Incomplete,
                JobCaptureState::Unconfirmed,
                JobCaptureState::NotStarted,
                JobCaptureState::Interrupted,
            ]
            .map(|s| (s as u8, s.as_str())),
            [
                (1, "complete"),
                (2, "incomplete"),
                (3, "unconfirmed"),
                (4, "not_started"),
                (5, "interrupted"),
            ]
        );
        assert_eq!(
            [
                JobOrigin::Model {
                    operation: [1; 16],
                    generation: 1,
                    ordinal: 1
                }
                .code(),
                JobOrigin::Person { input: [1; 16] }.code()
            ],
            [1, 2]
        );
    }
    /// TJ-26: no job record holds output, a command or a path. Kinds 26 and
    /// 27 have fixed sizes; kind 28's size depends only on entry counts.
    #[test]
    fn j2t_01_job_records_hold_no_output_command_or_path() {
        assert_eq!(round_job(r26(intent26())).len(), 41);
        assert_eq!(
            round_job(Record::JobCancelRequested(cancel27(
                JobReason::ClientCancel
            )))
            .len(),
            19
        );
        // 37 job facts, 3 capture header, two streams with one kind on
        // stdout, one number, pause facts and the reservation.
        let bytes = round_job(r28(settled28()));
        assert_eq!(
            bytes.len(),
            37 + 3 + (101 + 9 + 8) + (101 + 8) + 6 + 4 + 20 + 8 + 8
        );
        assert_eq!(
            round_job(r28(JobSettled::interrupted([3; 16], 1_760_000_000_001))).len(),
            37 + 3 + 2 * 109 + 6 + 20 + 8 + 8
        );
    }

    // J3T-02 (unit): kind 29 JobResult (tool-job-controller.md, kind 29).
    fn snapshot29(state: JobRunState) -> JobSnapshot {
        JobSnapshot {
            state,
            numbers: [0; 3],
            elapsed_ms: 0,
            stdout_bytes: 0,
            stderr_bytes: 0,
        }
    }
    fn result29(outcome: JobResultOutcome, job: Id, body: JobResultBody) -> JobResult {
        JobResult {
            operation: [1; 16],
            generation: 1,
            ordinal: 1,
            status: outcome.status(),
            outcome,
            job,
            body,
        }
    }
    fn r29(p: JobResult) -> Record {
        Record::JobResult(Box::new(p))
    }
    fn entry29(n: u8, state: JobRunState) -> JobListEntry {
        JobListEntry {
            job: [n; 16],
            state,
            numbers: [0; 3],
            stdout_bytes: if state == JobRunState::Settled {
                0
            } else {
                u64::MAX
            },
            stderr_bytes: 0,
        }
    }
    fn page29(state: JobPageState, source_len: u32) -> JobPage {
        JobPage {
            stream: OutputStream::Stderr,
            start: 7,
            source_len,
            total: 7 + u64::from(source_len),
            state,
            digest: if source_len == 0 { [0; 32] } else { [4; 32] },
        }
    }
    #[test]
    fn j3t_02_kind_29_round_trips_every_bound_and_maximum_size() {
        use JobResultOutcome as O;
        // Every outcome with no body, at the field bounds.
        for outcome in O::ALL {
            for (generation, ordinal) in [(1, 1), (u64::MAX, MAX_RECORDED_CALLS)] {
                let job = if outcome == O::NotFound {
                    [0; 16]
                } else {
                    [0xff; 16]
                };
                let bytes = round_job(r29(JobResult {
                    generation,
                    ordinal,
                    ..result29(outcome, job, JobResultBody::None)
                }));
                assert_eq!(bytes.len(), JOB_RESULT_FIXED);
                assert_eq!((bytes[28], bytes[29]), (outcome.status(), outcome as u8));
            }
        }
        // The state block: every state and reason, the reason numbers and
        // the elapsed and byte bounds.
        let mut states = vec![
            JobRunState::Starting,
            JobRunState::Running,
            JobRunState::Stopping,
            JobRunState::Draining,
            JobRunState::CleanupUnconfirmed,
            JobRunState::Finalizing,
            JobRunState::Settled,
            JobRunState::CapturePaused(JobPauseReason::Quota),
            JobRunState::CapturePaused(JobPauseReason::DiskSpace),
        ];
        states.extend(JobBlockReason::ALL.map(JobRunState::Queued));
        for state in states {
            let spawned = !state.before_spawn() && state != JobRunState::Settled;
            let snapshot = JobSnapshot {
                numbers: match state {
                    JobRunState::Queued(
                        JobBlockReason::AgentSlots | JobBlockReason::ServiceSlots,
                    ) => [255, 255, 255],
                    JobRunState::Queued(JobBlockReason::PendingInstalls) => [4, 0, 0],
                    _ => [0; 3],
                },
                elapsed_ms: if spawned { u64::MAX } else { 0 },
                stdout_bytes: if state == JobRunState::Settled {
                    0
                } else {
                    u64::MAX
                },
                stderr_bytes: if state == JobRunState::Settled {
                    0
                } else {
                    u64::MAX
                },
                ..snapshot29(state)
            };
            for outcome in [O::Ok, O::Cancelled] {
                let bytes = round_job(r29(result29(
                    outcome,
                    [0; 16],
                    JobResultBody::State(snapshot),
                )));
                assert_eq!(bytes.len(), JOB_RESULT_FIXED + JOB_STATE_BLOCK);
                assert_eq!((bytes[46], bytes[47]), (state.code(), state.reason_code()));
            }
            for end in [WaitEnd::NewOutput, WaitEnd::Time] {
                let bytes = round_job(r29(result29(
                    O::Ok,
                    [3; 16],
                    JobResultBody::Wait(snapshot, end),
                )));
                assert_eq!(bytes.len(), JOB_RESULT_FIXED + JOB_WAIT_BLOCK);
                assert_eq!(bytes[bytes.len() - 1], end as u8);
            }
        }
        for end in [WaitEnd::Settled, WaitEnd::AlreadySettled] {
            round_job(r29(result29(
                O::Ok,
                [3; 16],
                JobResultBody::Wait(snapshot29(JobRunState::Settled), end),
            )));
        }
        // The page block: every page state, both streams, 0 and 8,192
        // source bytes.
        for state in JobPageState::ALL {
            let lengths: &[u32] = if state.readable() {
                &[0, 1, 8192]
            } else {
                &[0]
            };
            for &length in lengths {
                for stream in [OutputStream::Stdout, OutputStream::Stderr] {
                    let page = JobPage {
                        stream,
                        ..page29(state, length)
                    };
                    let bytes = round_job(r29(result29(O::Ok, [3; 16], JobResultBody::Page(page))));
                    assert_eq!(bytes.len(), JOB_RESULT_FIXED + JOB_PAGE_BLOCK);
                    assert_eq!(page.next_offset(), 7 + u64::from(length));
                }
            }
        }
        // A page at the end of a live stream: no bytes, at the total.
        round_job(r29(result29(
            O::Ok,
            [3; 16],
            JobResultBody::Page(JobPage {
                start: 100,
                total: 50,
                ..page29(JobPageState::Live, 0)
            }),
        )));
        // The list block: 0 to 8 entries; `more` only with 8; the maximum.
        for count in 0..=MAX_JOB_LIST_ENTRIES as u8 {
            let entries: Vec<_> = (1..=count)
                .map(|n| entry29(n, JobRunState::Running))
                .collect();
            for more in [false, true] {
                let record = r29(result29(
                    O::Ok,
                    [0; 16],
                    JobResultBody::List {
                        more,
                        entries: entries.clone(),
                    },
                ));
                if more && usize::from(count) < MAX_JOB_LIST_ENTRIES {
                    invalid_job(record);
                } else {
                    let bytes = round_job(record);
                    assert_eq!(
                        bytes.len(),
                        JOB_RESULT_FIXED + 2 + usize::from(count) * JOB_LIST_ENTRY
                    );
                }
            }
        }
        let max = r29(result29(
            O::Ok,
            [0; 16],
            JobResultBody::List {
                more: true,
                entries: (1..=8)
                    .map(|n| JobListEntry {
                        numbers: [255, 255, 255],
                        ..entry29(n, JobRunState::Queued(JobBlockReason::ServiceSlots))
                    })
                    .collect(),
            },
        ));
        let bytes = round_job(max.clone());
        assert_eq!(bytes.len(), MAX_JOB_RESULT_PAYLOAD);
        assert_eq!(MAX_JOB_RESULT_PAYLOAD, 344);
        assert_eq!(encode_frame(&context(), &max).unwrap().len(), 528);
        // The outcome and status table (field 4 is fixed by field 5).
        let table = [
            (O::Ok, 1, "ok"),
            (O::NotFound, 3, "not_found"),
            (O::NotPermitted, 2, "not_permitted"),
            (O::Busy, 4, "busy"),
            (O::Cancelled, 6, "cancelled"),
            (O::Unavailable, 4, "unavailable"),
            (O::Limit, 7, "limit"),
        ];
        for (index, (outcome, status, name)) in table.into_iter().enumerate() {
            assert_eq!(O::from_code(index as u8 + 1), Some(outcome));
            assert_eq!((outcome.status(), outcome.as_str()), (status, name));
        }
        assert_eq!(O::from_code(0), None);
        assert_eq!(O::from_code(8), None);
        // Allowed codes per outcome (the outcome table).
        use ToolOperation as T;
        let codes = [
            T::ExecCmd,
            T::JobList,
            T::JobStatus,
            T::JobRead,
            T::JobWait,
            T::JobCancel,
            T::JobNote,
        ];
        let allowed = |outcome: O| {
            codes
                .iter()
                .filter(|c| outcome.allows(**c))
                .map(|c| *c as u8)
                .collect::<Vec<_>>()
        };
        assert_eq!(allowed(O::Ok), vec![10, 11, 12, 13, 14, 15, 16]);
        assert_eq!(allowed(O::NotFound), vec![12, 13, 14, 15, 16]);
        assert_eq!(allowed(O::NotPermitted), vec![15]);
        assert_eq!(allowed(O::Busy), vec![13]);
        assert_eq!(allowed(O::Cancelled), vec![10, 11, 12, 13, 14, 16]);
        assert_eq!(allowed(O::Unavailable), vec![11, 12, 13, 14, 15, 16]);
        assert_eq!(allowed(O::Limit), vec![16]);
        for outcome in O::ALL {
            assert!(!outcome.allows(T::ReadFile) && !outcome.allows(T::CreateNote));
        }
    }
    #[test]
    fn j3t_02_kind_29_rejects_each_invalid_field() {
        use JobResultOutcome as O;
        let state = |s| JobResultBody::State(snapshot29(s));
        let list = |more, entries| JobResultBody::List { more, entries };
        for p in [
            JobResult {
                operation: [0; 16],
                ..result29(O::Ok, [0; 16], JobResultBody::None)
            },
            JobResult {
                generation: 0,
                ..result29(O::Ok, [0; 16], JobResultBody::None)
            },
            JobResult {
                ordinal: 0,
                ..result29(O::Ok, [0; 16], JobResultBody::None)
            },
            JobResult {
                ordinal: MAX_RECORDED_CALLS + 1,
                ..result29(O::Ok, [0; 16], JobResultBody::None)
            },
            // Status fixed by the outcome.
            JobResult {
                status: 2,
                ..result29(O::Ok, [0; 16], JobResultBody::None)
            },
            JobResult {
                status: 4,
                ..result29(O::NotFound, [0; 16], JobResultBody::None)
            },
            // not_found names no job.
            result29(O::NotFound, [1; 16], JobResultBody::None),
            // Bodies only with the outcomes that carry them.
            result29(O::Busy, [1; 16], state(JobRunState::Running)),
            result29(
                O::Cancelled,
                [1; 16],
                JobResultBody::Wait(snapshot29(JobRunState::Running), WaitEnd::Time),
            ),
            result29(
                O::Unavailable,
                [1; 16],
                JobResultBody::Page(page29(JobPageState::Live, 1)),
            ),
            result29(O::Limit, [0; 16], list(false, vec![])),
            // State block rules.
            result29(
                O::Ok,
                [0; 16],
                JobResultBody::State(JobSnapshot {
                    elapsed_ms: 1,
                    ..snapshot29(JobRunState::Queued(JobBlockReason::Quota))
                }),
            ),
            result29(
                O::Ok,
                [0; 16],
                JobResultBody::State(JobSnapshot {
                    elapsed_ms: 1,
                    ..snapshot29(JobRunState::Starting)
                }),
            ),
            result29(
                O::Ok,
                [0; 16],
                JobResultBody::State(JobSnapshot {
                    elapsed_ms: 1,
                    ..snapshot29(JobRunState::Settled)
                }),
            ),
            result29(
                O::Ok,
                [0; 16],
                JobResultBody::State(JobSnapshot {
                    stdout_bytes: 1,
                    ..snapshot29(JobRunState::Settled)
                }),
            ),
            result29(
                O::Ok,
                [0; 16],
                JobResultBody::State(JobSnapshot {
                    stderr_bytes: 1,
                    ..snapshot29(JobRunState::Settled)
                }),
            ),
            result29(
                O::Ok,
                [0; 16],
                JobResultBody::State(JobSnapshot {
                    numbers: [1, 0, 0],
                    ..snapshot29(JobRunState::Running)
                }),
            ),
            result29(
                O::Ok,
                [0; 16],
                JobResultBody::State(JobSnapshot {
                    numbers: [4, 1, 0],
                    ..snapshot29(JobRunState::Queued(JobBlockReason::PendingInstalls))
                }),
            ),
            result29(
                O::Ok,
                [0; 16],
                JobResultBody::State(JobSnapshot {
                    numbers: [0, 0, 1],
                    ..snapshot29(JobRunState::Queued(JobBlockReason::Quota))
                }),
            ),
            // A wait that ended on settlement reports state 9.
            result29(
                O::Ok,
                [3; 16],
                JobResultBody::Wait(snapshot29(JobRunState::Running), WaitEnd::Settled),
            ),
            result29(
                O::Ok,
                [3; 16],
                JobResultBody::Wait(snapshot29(JobRunState::Running), WaitEnd::AlreadySettled),
            ),
            // Page block rules.
            result29(
                O::Ok,
                [3; 16],
                JobResultBody::Page(page29(JobPageState::Live, 8193)),
            ),
            result29(
                O::Ok,
                [3; 16],
                JobResultBody::Page(JobPage {
                    total: 7,
                    ..page29(JobPageState::Complete, 1)
                }),
            ),
            result29(
                O::Ok,
                [3; 16],
                JobResultBody::Page(JobPage {
                    start: u64::MAX,
                    total: u64::MAX,
                    ..page29(JobPageState::Live, 1)
                }),
            ),
            result29(
                O::Ok,
                [3; 16],
                JobResultBody::Page(JobPage {
                    digest: [0; 32],
                    ..page29(JobPageState::Live, 1)
                }),
            ),
            result29(
                O::Ok,
                [3; 16],
                JobResultBody::Page(JobPage {
                    digest: [4; 32],
                    ..page29(JobPageState::Live, 0)
                }),
            ),
            // List block rules.
            result29(O::Ok, [1; 16], list(false, vec![])),
            result29(
                O::Ok,
                [0; 16],
                list(
                    false,
                    vec![
                        entry29(1, JobRunState::Running),
                        entry29(1, JobRunState::Starting),
                    ],
                ),
            ),
            result29(
                O::Ok,
                [0; 16],
                list(false, vec![entry29(0, JobRunState::Running)]),
            ),
            result29(
                O::Ok,
                [0; 16],
                list(
                    false,
                    vec![JobListEntry {
                        stdout_bytes: 1,
                        ..entry29(1, JobRunState::Settled)
                    }],
                ),
            ),
            result29(
                O::Ok,
                [0; 16],
                list(
                    false,
                    vec![JobListEntry {
                        numbers: [0, 1, 0],
                        ..entry29(1, JobRunState::Running)
                    }],
                ),
            ),
            result29(
                O::Ok,
                [0; 16],
                list(true, vec![entry29(1, JobRunState::Running)]),
            ),
        ] {
            invalid_job(r29(p));
        }
        // Nine entries cannot be encoded as a valid list.
        assert_eq!(
            payload(&r29(result29(
                O::Ok,
                [0; 16],
                list(
                    false,
                    (1..=9).map(|n| entry29(n, JobRunState::Running)).collect()
                ),
            ))),
            Err(ReplayError::Corrupt)
        );
        // Wire-only values: unknown codes and sizes of no body form.
        let good = raw(&r29(result29(
            O::Ok,
            [3; 16],
            JobResultBody::Wait(snapshot29(JobRunState::Running), WaitEnd::Time),
        )));
        for (at, value) in [
            (29, 0),
            (29, 8),
            (46, 0),
            (46, 10),
            (47, 1),
            (75, 0),
            (75, 5),
        ] {
            let mut bytes = good.clone();
            bytes[at] = value;
            assert_eq!(decode_record(29, &bytes), Err(ReplayError::Corrupt), "{at}");
        }
        let page = raw(&r29(result29(
            O::Ok,
            [3; 16],
            JobResultBody::Page(page29(JobPageState::Live, 1)),
        )));
        for (at, value) in [(46, 0), (46, 3), (67, 0), (67, 9)] {
            let mut bytes = page.clone();
            bytes[at] = value;
            assert_eq!(decode_record(29, &bytes), Err(ReplayError::Corrupt), "{at}");
        }
        let listed = raw(&r29(result29(
            O::Ok,
            [0; 16],
            list(false, vec![entry29(1, JobRunState::Running)]),
        )));
        for (at, value) in [(46, 2), (46, 0), (47, 2)] {
            let mut bytes = listed.clone();
            bytes[at] = value;
            assert_eq!(decode_record(29, &bytes), Err(ReplayError::Corrupt), "{at}");
        }
        for len in [
            JOB_RESULT_FIXED - 1,
            JOB_RESULT_FIXED + 1,
            JOB_RESULT_FIXED + 31,
        ] {
            let mut bytes = good.clone();
            bytes.resize(len, 0);
            assert_eq!(
                decode_record(29, &bytes),
                Err(ReplayError::Corrupt),
                "{len}"
            );
        }
        let mut bytes = raw(&r29(result29(
            O::Ok,
            [0; 16],
            JobResultBody::List {
                more: true,
                entries: (1..=8).map(|n| entry29(n, JobRunState::Running)).collect(),
            },
        )));
        bytes.push(0);
        assert_eq!(decode_record(29, &bytes), Err(ReplayError::Corrupt));
    }
    /// TJ-26, J3T-17: kind 29 has no text field; its size depends only on
    /// the body form and the entry count.
    #[test]
    fn j3t_02_kind_29_holds_no_text() {
        use JobResultOutcome as O;
        for body in [
            JobResultBody::None,
            JobResultBody::State(snapshot29(JobRunState::Running)),
            JobResultBody::Page(page29(JobPageState::Complete, 8192)),
            JobResultBody::Wait(snapshot29(JobRunState::Settled), WaitEnd::Settled),
        ] {
            let size = match &body {
                JobResultBody::None => 0,
                JobResultBody::State(_) => 29,
                JobResultBody::Page(_) => 54,
                _ => 30,
            };
            let bytes = round_job(r29(result29(O::Ok, [3; 16], body)));
            assert_eq!(bytes.len(), 46 + size);
        }
    }

    // J4T-02 (unit): kind 30 JobRetention.
    fn person30(action: RetentionAction, note: &str) -> JobRetention {
        JobRetention {
            job: [3; 16],
            action,
            author: RetentionAuthor::Person { request: [4; 16] },
            time: 1_760_000_000_003,
            note: note.into(),
        }
    }
    fn model30() -> JobRetention {
        JobRetention {
            job: [3; 16],
            action: RetentionAction::Note,
            author: RetentionAuthor::Model {
                operation: [5; 16],
                generation: 1,
                ordinal: 1,
            },
            time: 1_760_000_000_004,
            note: String::new(),
        }
    }
    fn r30(p: JobRetention) -> Record {
        Record::JobRetention(Box::new(p))
    }
    #[test]
    fn j4t_02_kind_30_round_trips_every_bound_and_maximum_size() {
        use RetentionAction::*;
        for action in [Pin, Unpin, Delete] {
            let bytes = round_job(r30(person30(action, "")));
            assert_eq!(bytes.len(), 16 + 1 + 1 + 8 + 16 + 4);
            assert_eq!((bytes[16], bytes[17]), (action as u8, 1));
        }
        // A person note after display redaction: the unnumbered placeholder
        // only, 1 to 2,048 bytes.
        for note in [
            "x".to_owned(),
            "rotate $ASURA_SECRET_0 before release".to_owned(),
            "é".repeat(MAX_JOB_NOTE / 2),
        ] {
            let bytes = round_job(r30(person30(Note, &note)));
            assert_eq!(bytes.len(), 46 + note.len());
            assert!(bytes.ends_with(note.as_bytes()));
        }
        let max = r30(person30(Note, &"n".repeat(MAX_JOB_NOTE)));
        assert_eq!(round_job(max.clone()).len(), MAX_JOB_RETENTION_PAYLOAD);
        assert_eq!(MAX_JOB_RETENTION_PAYLOAD, 2_094);
        assert_eq!(encode_frame(&context(), &max).unwrap().len(), 2_278);
        // A model note cites its kind 14 and holds no text (TCR-O5).
        let model = r30(model30());
        let bytes = round_job(model.clone());
        assert_eq!(bytes.len(), MODEL_NOTE_PAYLOAD);
        assert_eq!(MODEL_NOTE_PAYLOAD, 58);
        assert_eq!(encode_frame(&context(), &model).unwrap().len(), 242);
        assert_eq!((bytes[16], bytes[17]), (4, 2));
        round_job(r30(JobRetention {
            author: RetentionAuthor::Model {
                operation: [0xff; 16],
                generation: u64::MAX,
                ordinal: MAX_RECORDED_CALLS,
            },
            ..model30()
        }));
        for (code, name) in [(1, "pin"), (2, "unpin"), (3, "delete"), (4, "note")] {
            let action = RetentionAction::from_code(code).unwrap();
            assert_eq!((action as u8, action.as_str()), (code, name));
        }
        assert_eq!(RetentionAction::from_code(0), None);
        assert_eq!(RetentionAction::from_code(5), None);
    }
    #[test]
    fn j4t_02_kind_30_rejects_each_invalid_field() {
        use RetentionAction::*;
        let model = |change: fn(&mut JobRetention)| {
            let mut p = model30();
            change(&mut p);
            p
        };
        for p in [
            JobRetention {
                job: [0; 16],
                ..person30(Pin, "")
            },
            person30(Note, ""),
            person30(Pin, "text"),
            person30(Delete, "text"),
            JobRetention {
                author: RetentionAuthor::Person { request: [0; 16] },
                ..person30(Pin, "")
            },
            // The model writes only notes, without text.
            model(|p| p.action = Pin),
            model(|p| p.action = Delete),
            model(|p| p.note = "copy".into()),
            model(|p| {
                p.author = RetentionAuthor::Model {
                    operation: [0; 16],
                    generation: 1,
                    ordinal: 1,
                }
            }),
            model(|p| {
                p.author = RetentionAuthor::Model {
                    operation: [5; 16],
                    generation: 0,
                    ordinal: 1,
                }
            }),
            model(|p| {
                p.author = RetentionAuthor::Model {
                    operation: [5; 16],
                    generation: 1,
                    ordinal: 0,
                }
            }),
            model(|p| {
                p.author = RetentionAuthor::Model {
                    operation: [5; 16],
                    generation: 1,
                    ordinal: MAX_RECORDED_CALLS + 1,
                }
            }),
        ] {
            invalid_job(r30(p));
        }
        // A note over 2,048 bytes is over the limit to encode and Corrupt to
        // decode.
        let long = person30(Note, &"n".repeat(MAX_JOB_NOTE + 1));
        assert_eq!(payload(&r30(long.clone())), Err(ReplayError::Limit));
        assert_eq!(
            decode_record(30, &raw(&r30(long))),
            Err(ReplayError::Corrupt)
        );
        // Wire-only values: action, author and the note length.
        let good = raw(&r30(person30(Pin, "")));
        for (at, value) in [(16, 0), (16, 5), (17, 0), (17, 3)] {
            let mut bytes = good.clone();
            bytes[at] = value;
            assert_eq!(decode_record(30, &bytes), Err(ReplayError::Corrupt), "{at}");
        }
        let mut bytes = raw(&r30(model30()));
        bytes[17] = 1;
        assert_eq!(decode_record(30, &bytes), Err(ReplayError::Corrupt));
        let mut bytes = good;
        bytes.push(0);
        assert_eq!(decode_record(30, &bytes), Err(ReplayError::Corrupt));
        let mut bytes = raw(&r30(person30(Note, "ab")));
        bytes.pop();
        assert_eq!(decode_record(30, &bytes), Err(ReplayError::Corrupt));
        // Invalid UTF-8 note text.
        let mut bytes = raw(&r30(person30(Note, "ab")));
        let last = bytes.len() - 1;
        bytes[last] = 0xff;
        assert_eq!(decode_record(30, &bytes), Err(ReplayError::Corrupt));
    }
    /// J4T-02: the retention summary rules that replay and the writer apply.
    #[test]
    fn j4t_02_retention_summary_rules_and_bounds() {
        use RetentionAction::*;
        let mut s = JobRetentionSummary::default();
        assert!(s.allows(Pin, false) && !s.allows(Unpin, false));
        assert!(!s.allows(Delete, false) && s.allows(Delete, true));
        assert!(s.allows(Note, false));
        s.apply(Pin);
        assert!(!s.allows(Pin, true) && s.allows(Unpin, true));
        s.apply(Delete);
        assert!(!s.allows(Delete, true) && s.allows(Note, true));
        let mut s = JobRetentionSummary::default();
        for _ in 0..MAX_JOB_NOTES {
            assert!(s.has_room(Note));
            s.apply(Note);
        }
        assert!(!s.has_room(Note) && s.has_room(Pin));
        while (s.records as usize) < MAX_JOB_RETENTION_RECORDS {
            s.apply(if s.pinned { Unpin } else { Pin });
        }
        assert!(!s.has_room(Pin) && !s.has_room(Delete));
    }
}
