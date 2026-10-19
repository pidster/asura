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
    fn u16(&mut self, n: u16) {
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
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.array()?))
    }
    /// A J1 string: a length over `limit` is `Corrupt`, like any other field.
    fn bounded_string(&mut self, limit: usize) -> Result<String> {
        let n = self.u32()? as usize;
        if n > limit {
            return Err(ReplayError::Corrupt);
        }
        Ok(std::str::from_utf8(self.take(n)?)
            .map_err(|_| ReplayError::Corrupt)?
            .to_owned())
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
/// The `asura.exec_cmd.job` identity of one exec_cmd call: the first 16 bytes of
/// SHA-256 over ASCII `asura.exec_cmd.job`, one zero byte, the operation ID, the
/// generation and the ordinal (big endian). An all-zero result becomes 1 in its
/// final byte. Kind 22 and its job use this derivation.
pub fn exec_cmd_job_id(operation: Id, generation: u64, ordinal: u32) -> Id {
    let mut hash = Sha256::new();
    hash.update(b"asura.exec_cmd.job\0");
    hash.update(operation);
    hash.update(generation.to_be_bytes());
    hash.update(ordinal.to_be_bytes());
    let digest = hash.finalize();
    let mut id = [0; 16];
    id.copy_from_slice(&digest[..16]);
    if id == [0; 16] {
        id[15] = 1;
    }
    id
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
/// Maximum kind 23 payload (`tool-job-controller.md`, J1 records; TCR-D3).
pub(crate) const MAX_CAPTURE_SETTLED_PAYLOAD: usize = 4_066;
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
fn busy_from(code: u8) -> Result<Option<ExecCmdBusyReason>> {
    Ok(match code {
        0 => None,
        1 => Some(ExecCmdBusyReason::Quota),
        2 => Some(ExecCmdBusyReason::CapturePaused),
        3 => Some(ExecCmdBusyReason::PendingFull),
        4 => Some(ExecCmdBusyReason::PayloadUnavailable),
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
                        )
                    )
            }
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
                            && self.number_kinds.len() <= MAX_EXEC_CMD_VARIABLES))
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
        } && self.busy.is_some() == (decision == D::Busy)
            && (named
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
        let refusal = match self.refusal {
            None => !self.command_cut,
            Some(r) => {
                decision == D::Refused
                    && exec
                    && rules
                    && r.code != RefusalCode::DetectionUnavailable
                    && r.word.is_none_or(|w| (1..=MAX_REFUSAL_WORD).contains(&w))
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
    w.byte(p.busy.map_or(0, |b| b as u8));
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
    w.byte(u8::from(p.command_cut));
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
    let busy = busy_from(r.byte()?)?;
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
    let command_cut = match r.byte()? {
        0 => false,
        1 => true,
        _ => return Err(ReplayError::Corrupt),
    };
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
        busy,
        form,
        arguments_bytes,
        arguments,
        number_kinds,
        unnumbered,
        rule_set_revision,
        rule_set_digest,
        command_cut,
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
impl CaptureSettled {
    /// Every kind 23 field rule that the record alone determines. Replay adds
    /// the pending `run` exec_cmd kind 14 that it settles.
    pub(crate) fn validate(&self) -> Result<()> {
        let streams = [&self.stdout, &self.stderr];
        let digests = streams.iter().all(|s| s.digest.is_some());
        let no_digest = streams.iter().all(|s| s.digest.is_none());
        let state = match self.state {
            CaptureState::Complete => {
                self.reason == CaptureReason::None
                    && digests
                    && streams.iter().all(|s| s.eof && s.discarded_bytes == 0)
            }
            CaptureState::Incomplete => {
                !matches!(
                    self.reason,
                    CaptureReason::None | CaptureReason::SettlementUnconfirmed
                ) && digests
            }
            CaptureState::Unconfirmed => {
                self.reason == CaptureReason::SettlementUnconfirmed && no_digest
            }
            CaptureState::NotStarted => {
                matches!(
                    self.reason,
                    CaptureReason::None | CaptureReason::StorageFault
                ) && self.exit_kind == 0
                    && no_digest
                    && streams
                        .iter()
                        .all(|s| s.observed_bytes == 0 && s.stored_bytes == 0)
            }
        };
        let exit = match self.exit_kind {
            0 => self.exit_value == 0,
            1 => self.exit_value <= 255,
            2 => (1..=127).contains(&self.exit_value),
            _ => false,
        };
        require(
            self.operation != [0; 16]
                && self.generation >= 1
                && (1..=MAX_RECORDED_CALLS).contains(&self.ordinal)
                && state
                && exit
                && streams.iter().all(|s| valid_stream(s))
                && self.distinct_numbers <= u32::from(MAX_CAPTURE_NUMBER)
                && self.numbers.len() <= MAX_CAPTURE_NUMBERS
                && self.numbers.len() as u64 <= u64::from(self.distinct_numbers)
                && self
                    .numbers
                    .iter()
                    .all(|n| (1..=MAX_CAPTURE_NUMBER).contains(&n.number))
                && self.numbers.windows(2).all(|p| p[0].number < p[1].number)
                && self.final_reservation.is_multiple_of(MIB),
        )
    }
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
fn write_capture_settled(w: &mut Writer, p: &CaptureSettled) -> Result<()> {
    p.validate()?;
    capture_settled_fields(w, p);
    Ok(())
}
/// Kind 23 field order. Callers validate first; tests use it to build invalid bytes.
fn capture_settled_fields(w: &mut Writer, p: &CaptureSettled) {
    w.bytes(&p.operation);
    w.u64(p.generation);
    w.u32(p.ordinal);
    w.byte(p.state as u8);
    w.byte(p.reason as u8);
    w.byte(p.exit_kind);
    w.u32(p.exit_value);
    // Field 8, redaction ran: always 1 (SD9). The rule set is kind 14's
    // fields 18-19 (TCR-D3).
    w.byte(1);
    write_stream(w, &p.stdout);
    write_stream(w, &p.stderr);
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
}
fn read_capture_settled(r: &mut Reader<'_>) -> Result<CaptureSettled> {
    let operation = r.array()?;
    let generation = r.u64()?;
    let ordinal = r.u32()?;
    let state = match r.byte()? {
        1 => CaptureState::Complete,
        2 => CaptureState::Incomplete,
        3 => CaptureState::Unconfirmed,
        4 => CaptureState::NotStarted,
        _ => return Err(ReplayError::Corrupt),
    };
    let reason = match r.byte()? {
        0 => CaptureReason::None,
        1 => CaptureReason::StorageFault,
        2 => CaptureReason::Quota,
        3 => CaptureReason::Enospc,
        4 => CaptureReason::DetectorFault,
        5 => CaptureReason::DiscardedAfterCancel,
        6 => CaptureReason::SettlementUnconfirmed,
        _ => return Err(ReplayError::Corrupt),
    };
    let exit_kind = r.byte()?;
    let exit_value = r.u32()?;
    require(r.byte()? == 1)?;
    let stdout = read_stream(r)?;
    let stderr = read_stream(r)?;
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
    let value = CaptureSettled {
        operation,
        generation,
        ordinal,
        state,
        reason,
        exit_kind,
        exit_value,
        stdout,
        stderr,
        distinct_numbers,
        numbers,
        pause_count: r.u32()?,
        first_pause_ms: r.u64()?,
        paused_ms: r.u64()?,
        final_reservation: r.u64()?,
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
        Record::CaptureSettled(p) => write_capture_settled(&mut w, p)?,
        Record::ResponseText(p) => {
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
        // Kinds 17 and 22 are retired (TCR-O2); like any unknown kind they
        // are UnsupportedFormat.
        23 => {
            require(payload.len() <= MAX_CAPTURE_SETTLED_PAYLOAD)?;
            Record::CaptureSettled(Box::new(read_capture_settled(&mut r)?))
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
    #[test]
    fn exec_cmd_job_identity_has_fixed_domain() {
        let expected = [
            0x7e, 0x66, 0x8e, 0x9f, 0x8f, 0xb9, 0xa0, 0xd0, 0x12, 0x85, 0xb5, 0xc8, 0x42, 0x66,
            0x70, 0x02,
        ];
        assert_eq!(exec_cmd_job_id([1; 16], 1, 1), expected);
        for (operation, generation, ordinal) in [([2; 16], 1, 1), ([1; 16], 2, 1), ([1; 16], 1, 2)]
        {
            assert_ne!(exec_cmd_job_id(operation, generation, ordinal), expected);
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
            busy: None,
            form: ArgumentForm::Canonical,
            arguments_bytes: 48,
            arguments: "{\"command\":\"printf \\\"$ASURA_SECRET_1\\\"\"}".into(),
            number_kinds: vec![SecretKind::GithubToken],
            unnumbered: 0,
            rule_set_revision: 1,
            rule_set_digest: [5; 32],
            command_cut: false,
            refusal: None,
        }
    }
    fn refused14() -> ToolCall {
        ToolCall {
            decision: ToolDecision::Refused,
            arguments: "{\"command\":\"echo $ASURA_SECRET_1\"}".into(),
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
    /// TCR-U01: every field at each bound round trips through the real
    /// encoder and decoder.
    #[test]
    fn tcr_u01_kind_14_round_trips_every_field_bound() {
        round14(run14());
        round14(refused14());
        round14(invalid14());
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
        for (decision, fault, busy) in [
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
                busy,
                ..run14()
            });
        }
        for busy in [
            ExecCmdBusyReason::Quota,
            ExecCmdBusyReason::CapturePaused,
            ExecCmdBusyReason::PendingFull,
            ExecCmdBusyReason::PayloadUnavailable,
        ] {
            let bytes = round14(ToolCall {
                decision: ToolDecision::Busy,
                busy: Some(busy),
                ..run14()
            });
            assert_eq!(bytes[48], busy as u8);
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
        // Refusal bounds and the cut flag.
        round14(ToolCall {
            command_cut: true,
            refusal: Some(ExecCmdRefusal {
                code: RefusalCode::RewrittenCommandTooLong,
                kind: None,
                word: Some(4096),
            }),
            ..refused14()
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
            // Busy reason only with busy, and busy only for exec_cmd.
            ToolCall {
                busy: Some(ExecCmdBusyReason::Quota),
                ..run14()
            },
            ToolCall {
                decision: D::Busy,
                ..run14()
            },
            ToolCall {
                decision: D::Busy,
                busy: Some(ExecCmdBusyReason::Quota),
                ..invalid14()
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
        for (offset, value) in [(kinds, 0), (kinds, 34), (base.len() - 5, 2)] {
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
    fn settled() -> CaptureSettled {
        CaptureSettled {
            operation: [1; 16],
            generation: 1,
            ordinal: 1,
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
        capture_settled_fields(&mut w, p);
        w.0
    }
    fn round23(p: CaptureSettled) -> Vec<u8> {
        let record = Record::CaptureSettled(Box::new(p));
        let bytes = payload(&record).unwrap();
        assert_eq!(decode_record(23, &bytes).unwrap(), record);
        bytes
    }
    fn invalid23(p: CaptureSettled) {
        let record = Record::CaptureSettled(Box::new(p.clone()));
        assert_eq!(payload(&record), Err(ReplayError::Corrupt), "{p:?}");
        assert_eq!(
            decode_record(23, &raw23(&p)),
            Err(ReplayError::Corrupt),
            "{p:?}"
        );
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
    fn j1t_01_kind_23_round_trips_every_bound_and_maximum_size() {
        let origins = [NumberOrigin::Command, NumberOrigin::Output];
        let max = CaptureSettled {
            generation: u64::MAX,
            ordinal: 8,
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
        assert_eq!(bytes.len(), 4_066);
        assert_eq!(bytes.len(), MAX_CAPTURE_SETTLED_PAYLOAD);
        let frame = encode_frame(&context(), &Record::CaptureSettled(Box::new(max))).unwrap();
        assert_eq!(frame.len(), 4_250);
        // Minimum: no kinds, spans or numbers; 36 + 2 × 101 + 6 + 20 + 8.
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
        assert_eq!(round23(min).len(), 272);
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
            assert_eq!(bytes[28], state as u8);
            assert_eq!(bytes[29], reason as u8);
        }
        for (exit_kind, exit_value) in [(0, 0), (1, 0), (1, 255), (2, 1), (2, 127)] {
            round23(CaptureSettled {
                exit_kind,
                exit_value,
                ..settled()
            });
        }
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
        assert_eq!(digest[36 + 16], 1);
        assert_eq!(&digest[36 + 17..36 + 49], &[6; 32]);
        assert_eq!(digest[35], 1, "redaction ran");
    }
    #[test]
    fn j1t_01_kind_23_rejects_each_invalid_field() {
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
                operation: [0; 16],
                ..s()
            },
            CaptureSettled {
                generation: 0,
                ..s()
            },
            CaptureSettled { ordinal: 0, ..s() },
            CaptureSettled {
                ordinal: MAX_RECORDED_CALLS + 1,
                ..s()
            },
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
        // Wire-only values in the default record. stdout starts at 36 and has
        // one kind entry (110 bytes): kind count +73, loss count +83, span
        // count +99, degraded +100, discarded +101, EOF +109. stderr starts at
        // 146 with no entries: EOF +100. Numbers follow at 247.
        let stdout = 36;
        let stderr = 146;
        let numbers_at = stderr + 101;
        let wire: [(usize, u8); 20] = [
            (28, 0),
            (28, 5),
            (29, 7),
            (35, 0),
            (35, 2),
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
        assert!(decode_record(23, &base).is_ok());
        for (offset, value) in wire {
            let mut bytes = base.clone();
            assert_ne!(bytes[offset], value, "{offset}");
            bytes[offset] = value;
            assert_eq!(
                decode_record(23, &bytes),
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
        assert!(decode_record(23, &bytes).is_ok());
        bytes[stdout + 17] = 1;
        assert_eq!(decode_record(23, &bytes), Err(ReplayError::Corrupt));
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
            assert!(decode_record(23, &bytes).is_ok());
            // The first span starts after the span count, at stdout + 100.
            assert_eq!(bytes[stdout + 100 + 16], 3);
            bytes[stdout + 100 + 16] = 1;
            assert!(decode_record(23, &bytes).is_ok());
            bytes[stdout + 100 + 16] = reason;
            assert_eq!(decode_record(23, &bytes), Err(ReplayError::Corrupt));
        }
        let mut bytes = base.clone();
        bytes.push(0);
        assert_eq!(decode_record(23, &bytes), Err(ReplayError::Corrupt));
        assert_eq!(
            decode_record(23, &base[..base.len() - 1]),
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
            if variant != RefusalCode::DetectionUnavailable {
                let bytes = round14(ToolCall {
                    refusal: Some(ExecCmdRefusal {
                        code: variant,
                        kind: None,
                        word: None,
                    }),
                    ..refused14()
                });
                assert_eq!(bytes[bytes.len() - 4], code);
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
        ];
        for (index, name) in operations.iter().enumerate() {
            let op = ToolOperation::from_code(index as u8 + 1).unwrap();
            assert_eq!((op as u8, op.as_str()), (index as u8 + 1, *name));
        }
        assert_eq!(ToolOperation::from_code(0), None);
        assert_eq!(ToolOperation::from_code(11), None);
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
        ];
        for (index, name) in faults.iter().enumerate() {
            let f = ToolFault::from_code(index as u8 + 1).unwrap();
            assert_eq!((f as u8, f.as_str()), (index as u8 + 1, *name));
        }
        assert_eq!(ToolFault::from_code(16), None);
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
        assert_eq!(TOOL_NAMES, ["project", "memory", "service", "exec_cmd"]);
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
        // Kind 23 has no string field: its size depends only on entry counts.
        let bytes = round23(settled());
        assert_eq!(bytes.len(), 36 + (101 + 9) + 101 + 6 + 4 + 20 + 8);
    }
}
