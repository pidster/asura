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
/// Kind 22 variables, kind 23 numbers and span entries.
const MAX_EXEC_CMD_VARIABLES: usize = 16;
const MAX_REFUSAL_WORD: u16 = 4096;
const MAX_CAPTURE_SPANS: u64 = 64;
const MAX_CAPTURE_NUMBER: u16 = 16_384;
const MAX_CAPTURE_NUMBERS: usize = 256;
const MIB: u64 = 1024 * 1024;
/// Maximum kind 22 and 23 payloads (`tool-job-controller.md`, J1 records).
pub(crate) const MAX_EXEC_CMD_INTENT_PAYLOAD: usize = 9_384;
pub(crate) const MAX_CAPTURE_SETTLED_PAYLOAD: usize = 4_102;

fn valid_cwd(cwd: &str) -> bool {
    !cwd.is_empty()
        && cwd.len() <= MAX_EXEC_CMD_CWD_BYTES
        && !cwd.contains('\0')
        && !cwd.starts_with('/')
        && !cwd.split('/').any(|part| part == "..")
}
impl ExecCmdIntent {
    /// Every kind 22 field rule that the record alone determines. Replay adds
    /// the state rules and recomputes the job and payload IDs.
    pub(crate) fn validate(&self) -> Result<()> {
        let admitted = self.disposition == ExecCmdDisposition::Admitted;
        let refused = self.disposition == ExecCmdDisposition::Refused;
        let busy = self.disposition == ExecCmdDisposition::Busy;
        let unavailable = self
            .refusal
            .is_some_and(|r| r.code == RefusalCode::DetectionUnavailable);
        let ids = [self.job, self.stdout_payload, self.stderr_payload];
        require(
            self.operation != [0; 16]
                && self.generation >= 1
                && (1..=8).contains(&self.ordinal)
                && (!self.command_truncated || refused)
                && !self.command.is_empty()
                && self.command.len() <= MAX_EXEC_CMD_COMMAND_BYTES
                && !self.command.contains('\0')
                && valid_cwd(&self.cwd)
                && self.refusal.is_some() == refused
                && self
                    .refusal
                    .is_none_or(|r| r.word.is_none_or(|w| (1..=MAX_REFUSAL_WORD).contains(&w)))
                && self.busy.is_some() == busy
                && self.variables.len() <= MAX_EXEC_CMD_VARIABLES
                && (admitted || self.variables.is_empty())
                && if admitted {
                    !ids.contains(&[0; 16]) && self.initial_reservation == INITIAL_RESERVATION
                } else {
                    ids == [[0; 16]; 3] && self.initial_reservation == 0
                }
                && if unavailable {
                    self.rule_set_revision == 0 && self.rule_set_digest == [0; 32]
                } else {
                    self.rule_set_revision >= 1 && self.rule_set_digest != [0; 32]
                },
        )
    }
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
    /// equality with the pending admitted kind 22.
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
                && (1..=8).contains(&self.ordinal)
                && state
                && exit
                // Kind 23 follows only an admitted kind 22, which always names
                // a rule set; replay checks equality with that record.
                && self.rule_set_revision >= 1
                && self.rule_set_digest != [0; 32]
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
fn write_exec_cmd_intent(w: &mut Writer, p: &ExecCmdIntent) -> Result<()> {
    p.validate()?;
    exec_cmd_intent_fields(w, p);
    Ok(())
}
/// Kind 22 field order. Callers validate first; tests use it to build invalid bytes.
fn exec_cmd_intent_fields(w: &mut Writer, p: &ExecCmdIntent) {
    w.bytes(&p.operation);
    w.u64(p.generation);
    w.u32(p.ordinal);
    w.byte(p.disposition as u8);
    w.byte(u8::from(p.command_truncated));
    w.u32(p.command.len() as u32);
    w.bytes(p.command.as_bytes());
    w.u32(p.cwd.len() as u32);
    w.bytes(p.cwd.as_bytes());
    w.byte(p.refusal.map_or(0, |r| refusal_code(r.code)));
    w.byte(p.refusal.and_then(|r| r.kind).map_or(0, kind_code));
    w.u16(p.refusal.and_then(|r| r.word).unwrap_or(0));
    w.byte(p.busy.map_or(0, |b| b as u8));
    w.byte(p.variables.len() as u8);
    for (index, kind) in p.variables.iter().enumerate() {
        w.byte(index as u8 + 1);
        w.byte(kind_code(*kind));
    }
    w.bytes(&p.job);
    w.bytes(&p.stdout_payload);
    w.bytes(&p.stderr_payload);
    w.u64(p.initial_reservation);
    w.u32(p.rule_set_revision);
    w.bytes(&p.rule_set_digest);
}
fn read_exec_cmd_intent(r: &mut Reader<'_>) -> Result<ExecCmdIntent> {
    let operation = r.array()?;
    let generation = r.u64()?;
    let ordinal = r.u32()?;
    let disposition = match r.byte()? {
        1 => ExecCmdDisposition::Admitted,
        2 => ExecCmdDisposition::Refused,
        3 => ExecCmdDisposition::Busy,
        _ => return Err(ReplayError::Corrupt),
    };
    let command_truncated = match r.byte()? {
        0 => false,
        1 => true,
        _ => return Err(ReplayError::Corrupt),
    };
    let command = r.bounded_string(MAX_EXEC_CMD_COMMAND_BYTES)?;
    let cwd = r.bounded_string(MAX_EXEC_CMD_CWD_BYTES)?;
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
    let busy = match r.byte()? {
        0 => None,
        1 => Some(ExecCmdBusyReason::Quota),
        2 => Some(ExecCmdBusyReason::CapturePaused),
        3 => Some(ExecCmdBusyReason::PendingFull),
        4 => Some(ExecCmdBusyReason::PayloadUnavailable),
        _ => return Err(ReplayError::Corrupt),
    };
    let count = r.byte()? as usize;
    require(count <= MAX_EXEC_CMD_VARIABLES)?;
    let mut variables = Vec::with_capacity(count);
    for index in 0..count {
        require(r.byte()? as usize == index + 1)?;
        variables.push(kind_from(r.byte()?)?);
    }
    let value = ExecCmdIntent {
        operation,
        generation,
        ordinal,
        disposition,
        command_truncated,
        command,
        cwd,
        refusal,
        busy,
        variables,
        job: r.array()?,
        stdout_payload: r.array()?,
        stderr_payload: r.array()?,
        initial_reservation: r.u64()?,
        rule_set_revision: r.u32()?,
        rule_set_digest: r.array()?,
    };
    value.validate()?;
    Ok(value)
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
    // Field 8, redaction ran: always 1 (SD9).
    w.byte(1);
    w.u32(p.rule_set_revision);
    w.bytes(&p.rule_set_digest);
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
    let rule_set_revision = r.u32()?;
    let rule_set_digest = r.array()?;
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
        rule_set_revision,
        rule_set_digest,
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
            w.u32(p.reserved_output_tokens);
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
            w.string(&p.path, 1024)?;
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
        Record::ExecCmdIntent(p) => write_exec_cmd_intent(&mut w, p)?,
        Record::CaptureSettled(p) => write_capture_settled(&mut w, p)?,
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
            let reserved_output_tokens = r.u32()?;
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
                reserved_output_tokens,
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
                path: r.string(1024)?,
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
        22 => {
            require(payload.len() <= MAX_EXEC_CMD_INTENT_PAYLOAD)?;
            Record::ExecCmdIntent(Box::new(read_exec_cmd_intent(&mut r)?))
        }
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
    /// Kind 14 never carries exec_cmd (tool kind 16); only kind 22 admits it.
    #[test]
    fn kind_14_rejects_exec_cmd_tool_kind() {
        let intent = ToolIntent {
            operation: [1; 16],
            generation: 1,
            ordinal: 1,
            kind: EXEC_CMD_TOOL_KIND,
            path: "printf hello\0.".into(),
            offset: 1000,
            limit: 30,
        };
        assert_eq!(valid_tool_intent(&intent), Err(ReplayError::Corrupt));
        let record = Record::ToolIntent(intent.clone());
        assert_eq!(payload(&record), Err(ReplayError::Corrupt));
        // A hand-built kind 14 payload with tool kind 16 also fails decode.
        let mut w = Writer(Vec::new());
        w.bytes(&intent.operation);
        w.u64(intent.generation);
        w.u32(intent.ordinal);
        w.byte(EXEC_CMD_TOOL_KIND);
        w.string("printf hello", 1024).unwrap();
        w.u64(0);
        w.u32(0);
        assert_eq!(decode_record(14, &w.0), Err(ReplayError::Corrupt));
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

    // J1T-01: kinds 22 and 23.
    fn admitted() -> ExecCmdIntent {
        ExecCmdIntent {
            operation: [1; 16],
            generation: 1,
            ordinal: 1,
            disposition: ExecCmdDisposition::Admitted,
            command_truncated: false,
            command: "printf \"$ASURA_SECRET_1\"".into(),
            cwd: ".".into(),
            refusal: None,
            busy: None,
            variables: vec![SecretKind::GithubToken],
            job: [2; 16],
            stdout_payload: [3; 16],
            stderr_payload: [4; 16],
            initial_reservation: INITIAL_RESERVATION,
            rule_set_revision: 1,
            rule_set_digest: [5; 32],
        }
    }
    fn refused(code: RefusalCode) -> ExecCmdIntent {
        let unavailable = code == RefusalCode::DetectionUnavailable;
        ExecCmdIntent {
            disposition: ExecCmdDisposition::Refused,
            refusal: Some(ExecCmdRefusal {
                code,
                kind: None,
                word: None,
            }),
            variables: vec![],
            job: [0; 16],
            stdout_payload: [0; 16],
            stderr_payload: [0; 16],
            initial_reservation: 0,
            rule_set_revision: if unavailable { 0 } else { 1 },
            rule_set_digest: if unavailable { [0; 32] } else { [5; 32] },
            ..admitted()
        }
    }
    fn busy(reason: ExecCmdBusyReason) -> ExecCmdIntent {
        ExecCmdIntent {
            disposition: ExecCmdDisposition::Busy,
            refusal: None,
            busy: Some(reason),
            ..refused(RefusalCode::UnparsedCommand)
        }
    }
    fn raw22(p: &ExecCmdIntent) -> Vec<u8> {
        let mut w = Writer(Vec::new());
        exec_cmd_intent_fields(&mut w, p);
        w.0
    }
    fn round22(p: ExecCmdIntent) -> Vec<u8> {
        let record = Record::ExecCmdIntent(Box::new(p));
        let bytes = payload(&record).unwrap();
        assert_eq!(decode_record(22, &bytes).unwrap(), record);
        bytes
    }
    /// Invalid on encode and, written without validation, on decode.
    fn invalid22(p: ExecCmdIntent) {
        let record = Record::ExecCmdIntent(Box::new(p.clone()));
        assert_eq!(payload(&record), Err(ReplayError::Corrupt), "{p:?}");
        assert_eq!(
            decode_record(22, &raw22(&p)),
            Err(ReplayError::Corrupt),
            "{p:?}"
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
    #[test]
    fn j1t_01_kind_22_round_trips_every_bound_and_maximum_size() {
        let max = ExecCmdIntent {
            generation: u64::MAX,
            ordinal: 8,
            command: "x".repeat(MAX_EXEC_CMD_COMMAND_BYTES),
            cwd: "y".repeat(MAX_EXEC_CMD_CWD_BYTES),
            variables: SecretKind::ALL[..16].to_vec(),
            rule_set_revision: u32::MAX,
            ..admitted()
        };
        let bytes = round22(max.clone());
        assert_eq!(bytes.len(), 9_384);
        assert_eq!(bytes.len(), MAX_EXEC_CMD_INTENT_PAYLOAD);
        let frame = encode_frame(&context(), &Record::ExecCmdIntent(Box::new(max))).unwrap();
        assert_eq!(frame.len(), 9_568);
        // The largest refused record is the same size: refusal fields are fixed.
        let refused_max = ExecCmdIntent {
            command_truncated: true,
            command: "x".repeat(MAX_EXEC_CMD_COMMAND_BYTES),
            cwd: "y".repeat(MAX_EXEC_CMD_CWD_BYTES),
            refusal: Some(ExecCmdRefusal {
                code: RefusalCode::SecretInCwd,
                kind: Some(SecretKind::WifiPassword),
                word: Some(4096),
            }),
            ..refused(RefusalCode::SecretInCwd)
        };
        assert_eq!(round22(refused_max).len(), 9_384 - 32);
        for ordinal in [1, 8] {
            round22(ExecCmdIntent {
                ordinal,
                ..admitted()
            });
        }
        for (command, cwd) in [
            ("x", "."),
            ("x", "a/b"),
            ("x", "./subdir"),
            ("x", "a//b/"),
            ("é", "..."),
            ("x", "a..b"),
        ] {
            round22(ExecCmdIntent {
                command: command.into(),
                cwd: cwd.into(),
                ..admitted()
            });
        }
        for count in 0..=16 {
            round22(ExecCmdIntent {
                variables: SecretKind::ALL[33 - count..].to_vec(),
                ..admitted()
            });
        }
        for kind in SecretKind::ALL {
            round22(ExecCmdIntent {
                variables: vec![kind; 16],
                ..admitted()
            });
        }
        for code in REFUSALS {
            for truncated in [false, true] {
                round22(ExecCmdIntent {
                    command_truncated: truncated,
                    ..refused(code)
                });
            }
        }
        for kind in SecretKind::ALL {
            for word in [None, Some(1), Some(4096)] {
                round22(ExecCmdIntent {
                    refusal: Some(ExecCmdRefusal {
                        code: RefusalCode::ResidualSecret,
                        kind: Some(kind),
                        word,
                    }),
                    ..refused(RefusalCode::ResidualSecret)
                });
            }
        }
        for reason in [
            ExecCmdBusyReason::Quota,
            ExecCmdBusyReason::CapturePaused,
            ExecCmdBusyReason::PendingFull,
            ExecCmdBusyReason::PayloadUnavailable,
        ] {
            let bytes = round22(ExecCmdIntent {
                command: "x".into(),
                ..busy(reason)
            });
            assert_eq!(bytes[38 + 1 + 1 + 4], reason as u8);
        }
    }
    #[test]
    fn j1t_01_kind_22_rejects_each_invalid_field() {
        let a = admitted;
        let r = || refused(RefusalCode::UnparsedCommand);
        let b = || busy(ExecCmdBusyReason::Quota);
        let unavailable = || refused(RefusalCode::DetectionUnavailable);
        for p in [
            ExecCmdIntent {
                operation: [0; 16],
                ..a()
            },
            ExecCmdIntent {
                generation: 0,
                ..a()
            },
            ExecCmdIntent { ordinal: 0, ..a() },
            ExecCmdIntent { ordinal: 9, ..a() },
            ExecCmdIntent {
                command_truncated: true,
                ..a()
            },
            ExecCmdIntent {
                command_truncated: true,
                ..b()
            },
            ExecCmdIntent {
                command: String::new(),
                ..a()
            },
            ExecCmdIntent {
                command: "x".repeat(8193),
                ..a()
            },
            ExecCmdIntent {
                command: "a\0b".into(),
                ..a()
            },
            ExecCmdIntent {
                cwd: String::new(),
                ..a()
            },
            ExecCmdIntent {
                cwd: "y".repeat(1025),
                ..a()
            },
            ExecCmdIntent {
                cwd: "/absolute".into(),
                ..a()
            },
            ExecCmdIntent {
                cwd: "..".into(),
                ..a()
            },
            ExecCmdIntent {
                cwd: "a/../b".into(),
                ..a()
            },
            ExecCmdIntent {
                cwd: "a/..".into(),
                ..a()
            },
            ExecCmdIntent {
                cwd: "a\0b".into(),
                ..a()
            },
            ExecCmdIntent {
                refusal: None,
                ..r()
            },
            ExecCmdIntent {
                refusal: r().refusal,
                ..a()
            },
            ExecCmdIntent {
                refusal: r().refusal,
                ..b()
            },
            ExecCmdIntent {
                refusal: Some(ExecCmdRefusal {
                    code: RefusalCode::UnparsedCommand,
                    kind: None,
                    word: Some(4097),
                }),
                ..r()
            },
            ExecCmdIntent { busy: None, ..b() },
            ExecCmdIntent {
                busy: Some(ExecCmdBusyReason::Quota),
                ..a()
            },
            ExecCmdIntent {
                busy: Some(ExecCmdBusyReason::Quota),
                ..r()
            },
            ExecCmdIntent {
                variables: vec![SecretKind::Jwt; 17],
                ..a()
            },
            ExecCmdIntent {
                variables: vec![SecretKind::Jwt],
                ..r()
            },
            ExecCmdIntent {
                variables: vec![SecretKind::Jwt],
                ..b()
            },
            ExecCmdIntent {
                job: [0; 16],
                ..a()
            },
            ExecCmdIntent {
                stdout_payload: [0; 16],
                ..a()
            },
            ExecCmdIntent {
                stderr_payload: [0; 16],
                ..a()
            },
            ExecCmdIntent {
                job: [2; 16],
                ..r()
            },
            ExecCmdIntent {
                stdout_payload: [3; 16],
                ..b()
            },
            ExecCmdIntent {
                stderr_payload: [4; 16],
                ..r()
            },
            ExecCmdIntent {
                initial_reservation: 0,
                ..a()
            },
            ExecCmdIntent {
                initial_reservation: 2 * INITIAL_RESERVATION,
                ..a()
            },
            ExecCmdIntent {
                initial_reservation: INITIAL_RESERVATION,
                ..r()
            },
            ExecCmdIntent {
                initial_reservation: INITIAL_RESERVATION,
                ..b()
            },
            ExecCmdIntent {
                rule_set_revision: 1,
                ..unavailable()
            },
            ExecCmdIntent {
                rule_set_digest: [5; 32],
                ..unavailable()
            },
            ExecCmdIntent {
                rule_set_revision: 0,
                ..r()
            },
            ExecCmdIntent {
                rule_set_digest: [0; 32],
                ..r()
            },
            ExecCmdIntent {
                rule_set_revision: 0,
                rule_set_digest: [0; 32],
                ..a()
            },
            ExecCmdIntent {
                rule_set_revision: 0,
                rule_set_digest: [0; 32],
                ..b()
            },
        ] {
            invalid22(p);
        }
        // Word 0 is "no word" on the wire, so `Some(0)` is an encode error only.
        let zero_word = ExecCmdIntent {
            refusal: Some(ExecCmdRefusal {
                code: RefusalCode::UnparsedCommand,
                kind: None,
                word: Some(0),
            }),
            ..r()
        };
        assert_eq!(
            payload(&Record::ExecCmdIntent(Box::new(zero_word))),
            Err(ReplayError::Corrupt)
        );
        // Wire-only values: offsets for command "x" (1 byte) and cwd "." (1 byte).
        let base = |p: ExecCmdIntent| {
            raw22(&ExecCmdIntent {
                command: "x".into(),
                cwd: ".".into(),
                ..p
            })
        };
        let refusal_at = 38 + 1 + 1;
        let wire: [(ExecCmdIntent, usize, u8); 17] = [
            (a(), 28, 0),
            (a(), 28, 4),
            (r(), 29, 2),
            (r(), 29, 3),
            (a(), 29, 2),
            (r(), refusal_at, 15),
            (r(), refusal_at, 255),
            (r(), refusal_at + 1, 34),
            (a(), refusal_at + 1, 1),
            (a(), refusal_at + 3, 1),
            (b(), refusal_at + 4, 5),
            (b(), refusal_at + 4, 0),
            (a(), refusal_at + 5, 2),
            (a(), refusal_at + 5, 17),
            (a(), refusal_at + 6, 2),
            (a(), refusal_at + 7, 0),
            (a(), refusal_at + 7, 34),
        ];
        for (p, offset, value) in wire {
            let mut bytes = base(p.clone());
            assert!(decode_record(22, &bytes).is_ok(), "{p:?}");
            bytes[offset] = value;
            assert_eq!(
                decode_record(22, &bytes),
                Err(ReplayError::Corrupt),
                "{offset} {value}"
            );
        }
        let mut bytes = base(a());
        bytes.push(0);
        assert_eq!(decode_record(22, &bytes), Err(ReplayError::Corrupt));
        let bytes = base(a());
        assert_eq!(
            decode_record(22, &bytes[..bytes.len() - 1]),
            Err(ReplayError::Corrupt)
        );
        let mut bytes = base(a());
        bytes[34] = 0xff;
        assert_eq!(decode_record(22, &bytes), Err(ReplayError::Corrupt));
        // A length over the field bound is `Corrupt`, not `Limit`.
        let mut bytes = base(a());
        bytes[30..34].copy_from_slice(&8193u32.to_be_bytes());
        assert_eq!(decode_record(22, &bytes), Err(ReplayError::Corrupt));
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
            rule_set_revision: 1,
            rule_set_digest: [5; 32],
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
            rule_set_revision: u32::MAX,
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
        assert_eq!(bytes.len(), 4_102);
        assert_eq!(bytes.len(), MAX_CAPTURE_SETTLED_PAYLOAD);
        let frame = encode_frame(&context(), &Record::CaptureSettled(Box::new(max))).unwrap();
        assert_eq!(frame.len(), 4_286);
        // Minimum: no kinds, spans or numbers; 72 + 2 × 101 + 6 + 20 + 8.
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
        assert_eq!(round23(min).len(), 308);
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
        assert_eq!(digest[72 + 16], 1);
        assert_eq!(&digest[72 + 17..72 + 49], &[6; 32]);
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
            CaptureSettled { ordinal: 9, ..s() },
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
                rule_set_revision: 0,
                ..s()
            },
            CaptureSettled {
                rule_set_digest: [0; 32],
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
        // Wire-only values in the default record. stdout starts at 72 and has
        // one kind entry (110 bytes): kind count +73, loss count +83, span
        // count +99, degraded +100, discarded +101, EOF +109. stderr starts at
        // 182 with no entries: EOF +100. Numbers follow at 283.
        let stdout = 72;
        let stderr = 182;
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
            // The stored byte, through the real encoder.
            let bytes = round22(ExecCmdIntent {
                command: "x".into(),
                cwd: ".".into(),
                ..refused(variant)
            });
            assert_eq!(bytes[40], code);
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
            let bytes = round22(ExecCmdIntent {
                command: "x".into(),
                cwd: ".".into(),
                variables: vec![kind],
                ..admitted()
            });
            assert_eq!(&bytes[46..48], &[1, code]);
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
        assert_eq!(ExecCmdDisposition::Admitted as u8, 1);
        assert_eq!(ExecCmdDisposition::Refused as u8, 2);
        assert_eq!(ExecCmdDisposition::Busy as u8, 3);
        assert_eq!(CaptureReason::SettlementUnconfirmed as u8, 6);
    }
    #[test]
    fn j1t_01_records_hold_no_value_or_value_digest() {
        // Kind 22 holds the rewritten command; no other variable-length field
        // exists. Everything outside command and cwd is structural.
        let rewritten = "curl -H \"Authorization: Bearer $ASURA_SECRET_1\" https://example.test";
        for count in [0, 1, 16] {
            let p = ExecCmdIntent {
                command: rewritten.into(),
                cwd: "sub/dir".into(),
                variables: vec![SecretKind::AuthHeader; count],
                ..admitted()
            };
            let bytes = round22(p.clone());
            assert_eq!(
                bytes.len(),
                30 + 4 + rewritten.len() + 4 + 7 + 6 + 2 * count + 48 + 8 + 36
            );
            assert_eq!(&bytes[34..34 + rewritten.len()], rewritten.as_bytes());
            assert_eq!(
                &bytes[38 + rewritten.len()..45 + rewritten.len()],
                b"sub/dir"
            );
            // Each variable entry is only (number, kind code).
            let vars = 45 + rewritten.len() + 6;
            for i in 0..count {
                assert_eq!(&bytes[vars + 2 * i..vars + 2 * i + 2], &[i as u8 + 1, 26]);
            }
            // The only 32-byte field is the rule-set digest, which is not a
            // value digest: it is the last field.
            assert_eq!(&bytes[bytes.len() - 32..], &[5; 32]);
        }
        // Kind 23 has no string field: its size depends only on entry counts.
        let bytes = round23(settled());
        assert_eq!(bytes.len(), 72 + (101 + 9) + 101 + 6 + 4 + 20 + 8);
    }
}
