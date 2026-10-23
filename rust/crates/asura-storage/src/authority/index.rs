//! Index entries of the segmented journal: fixed 32-byte keys and 96-byte
//! values that address settled history by frame locator. Entries are derived
//! from frames; they never hold prompt, response, argument or result text.
use super::ReplayError;
use super::conversation::{Cause, Hash, Id, Locator, TerminalKind};

pub mod run;

type Result<T> = std::result::Result<T, ReplayError>;

pub const KEY_BYTES: usize = 32;
pub const VALUE_BYTES: usize = 96;
pub const ENTRY_BYTES: usize = KEY_BYTES + VALUE_BYTES;

/// Index entry tags, declared in ascending byte order so that the derived
/// ordering of [`Key`] equals the byte order of its encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Tag {
    /// Project activity: project, accepted sequence.
    Activity = b'A',
    /// Model change of a project's build and test paths: project, kind 32
    /// action 1 sequence (P5).
    PathChange = b'B',
    /// Tool call: operation, ordinal.
    Call = b'C',
    /// Project identity: device and inode, 0.
    Identity = b'D',
    /// Job retention: job ID, kind 30 sequence (J4).
    Retention = b'E',
    /// Path warning acknowledgement: project, kind 32 action 2 sequence (P5).
    PathAck = b'F',
    /// Generated ID: installation, graph, task, operation, input or dispatch request.
    Generated = b'G',
    /// Thinking piece: operation, start offset (thinking-level.md).
    Thinking = b'H',
    /// Settled input.
    Input = b'I',
    /// Project registration.
    Project = b'J',
    /// Project conversation: project, creation sequence.
    ProjectConversation = b'K',
    /// Conversation job: conversation, kind 26 sequence (J4).
    ConversationJob = b'L',
    /// Project job: project, kind 26 sequence (J4).
    ProjectJob = b'M',
    /// Project name: project, name revision.
    Name = b'N',
    /// Terminal operation.
    Operation = b'O',
    /// Response piece: operation, start offset.
    Piece = b'P',
    /// Path approval or revocation: project, kind 32 action 3 or 4
    /// sequence (P5).
    PathApproval = b'Q',
    /// Request outcome.
    Request = b'R',
    /// Sealed segment file: first sequence.
    Segment = b'S',
    /// Turn: conversation, generation.
    Turn = b'T',
    /// Conversation settlement: conversation, kind 28 sequence (J4, J5).
    Settlement = b'U',
    /// Conversation.
    Conversation = b'V',
    /// Settled job: job ID, 0 (J2).
    Job = b'W',
    /// Retained payload of a job: payload ID, 0. The value's form byte is 2;
    /// form 1 (a J1 exec_cmd call) is retired with kind 23.
    Payload = b'X',
    /// Every job: the zero ID, kind 26 sequence (J4).
    AllJobs = b'Y',
    /// One kind 32 per reference: job, input or request ID, 0 (P5, AE-05).
    PathReference = b'Z',
    /// Conversation person command: conversation, kind 26 sequence (P3-E3).
    /// Lowercase because every uppercase letter is in use.
    PersonCommand = b'p',
}
impl Tag {
    pub const ALL: [Tag; 27] = [
        Tag::Activity,
        Tag::PathChange,
        Tag::Call,
        Tag::Identity,
        Tag::Retention,
        Tag::PathAck,
        Tag::Generated,
        Tag::Thinking,
        Tag::Input,
        Tag::Project,
        Tag::ProjectConversation,
        Tag::ConversationJob,
        Tag::ProjectJob,
        Tag::Name,
        Tag::Operation,
        Tag::Piece,
        Tag::PathApproval,
        Tag::Request,
        Tag::Segment,
        Tag::Turn,
        Tag::Settlement,
        Tag::Conversation,
        Tag::Job,
        Tag::Payload,
        Tag::AllJobs,
        Tag::PathReference,
        Tag::PersonCommand,
    ];
    pub fn from_byte(byte: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|tag| *tag as u8 == byte)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key {
    pub tag: Tag,
    pub id: Id,
    pub disc: u64,
}
impl Key {
    pub fn new(tag: Tag, id: Id, disc: u64) -> Self {
        Self { tag, id, disc }
    }
    /// The smallest key of a tag.
    pub fn first(tag: Tag) -> Self {
        Self::new(tag, [0; 16], 0)
    }
    /// The largest key of a tag.
    pub fn last(tag: Tag) -> Self {
        Self::new(tag, [0xff; 16], u64::MAX)
    }
    pub fn encode(&self) -> [u8; KEY_BYTES] {
        let mut out = [0; KEY_BYTES];
        out[0] = self.tag as u8;
        out[1..17].copy_from_slice(&self.id);
        out[17..25].copy_from_slice(&self.disc.to_be_bytes());
        out
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != KEY_BYTES || bytes[25..].iter().any(|b| *b != 0) {
            return Err(ReplayError::Corrupt);
        }
        Ok(Self {
            tag: Tag::from_byte(bytes[0]).ok_or(ReplayError::Corrupt)?,
            id: bytes[1..17].try_into().expect("16-byte id"),
            disc: u64::from_be_bytes(bytes[17..25].try_into().expect("8-byte discriminator")),
        })
    }
}
/// A sequence number as a 16-byte key ID (segment keys).
pub fn sequence_id(sequence: u64) -> Id {
    let mut id = [0; 16];
    id[8..].copy_from_slice(&sequence.to_be_bytes());
    id
}
/// Device and inode as a 16-byte key ID (identity keys).
pub fn identity_id(device: u64, inode: u64) -> Id {
    let mut id = [0; 16];
    id[..8].copy_from_slice(&device.to_be_bytes());
    id[8..].copy_from_slice(&inode.to_be_bytes());
    id
}

pub type Value = [u8; VALUE_BYTES];

/// Entity kinds of a [`Tag::Generated`] entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Generated {
    Installation = 1,
    Graph = 2,
    Task = 3,
    Operation = 4,
    Input = 5,
    Dispatch = 6,
}
impl Generated {
    pub fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            1 => Self::Installation,
            2 => Self::Graph,
            3 => Self::Task,
            4 => Self::Operation,
            5 => Self::Input,
            6 => Self::Dispatch,
            _ => return None,
        })
    }
}

/// Typed index values. Each encodes to at most 96 bytes, zero padded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    Request {
        kind: u16,
        digest: Hash,
        frame: Locator,
    },
    Generated {
        entity: Generated,
        frame: Locator,
    },
    Operation(OperationFacts),
    Piece {
        kept: u64,
        frame: Locator,
    },
    /// A thinking piece: the `Piece` layout over kind 25 frames.
    Thinking {
        kept: u64,
        frame: Locator,
    },
    /// `C`: a tool call's decision, kind 14 and result record (kind 15 or
    /// kind 29; `Locator::NONE` while absent).
    Call {
        decision: u8,
        intent: Locator,
        result: Locator,
    },
    /// `T`: one terminal turn. `notice_through` is its kind 21 notice
    /// cursor (J5).
    Turn {
        operation: Id,
        terminal: TerminalKind,
        accepted: Locator,
        completed: u64,
        notice_through: u64,
    },
    Input {
        status: u8,
        queued: Locator,
        promoted: Locator,
        operation: Id,
    },
    /// `X` form 2: a payload of a job (kinds 26 and 28). The last use is the
    /// kind 28 sequence.
    JobPayload {
        job: Id,
        intent: Locator,
        settled: Locator,
        last_use: u64,
    },
    /// `W`: a settled job. Kind 27 is `Locator::NONE` when absent.
    Job {
        conversation: Id,
        origin: u8,
        intent: Locator,
        cancel: Locator,
        settled: Locator,
    },
    /// `L`: a settled job of a conversation, keyed by its kind 26 sequence.
    ConversationJob {
        job: Id,
    },
    /// `M`: a settled job of a project, keyed by its kind 26 sequence.
    ProjectJob {
        job: Id,
        conversation: Id,
    },
    /// `Y`: a settled job of any project, keyed by its kind 26 sequence.
    AllJobs {
        job: Id,
        project: Id,
    },
    /// `U`: a job settlement of a conversation, keyed by its kind 28
    /// sequence.
    Settlement {
        job: Id,
    },
    /// `E`: one kind 30 of a job, keyed by its sequence.
    Retention {
        action: u8,
        author: u8,
        frame: Locator,
    },
    /// `B`: one kind 32 `model_changed` of a project, keyed by its
    /// sequence. `number` counts the project's `model_changed` records.
    PathChange {
        job: Id,
        number: u64,
        frame: Locator,
    },
    /// `F`: one kind 32 `acknowledged` of a project, keyed by its sequence.
    /// `covered` is the `number` of the change at `through`, or 0.
    PathAck {
        input: Id,
        through: u64,
        covered: u64,
        frame: Locator,
    },
    /// `Q`: one kind 32 `approved` or `revoked` of a project, keyed by its
    /// sequence. `path_key` is the SHA-256 path key; `digest` is zero for
    /// a revocation.
    PathApproval {
        action: u8,
        path_key: Hash,
        digest: Hash,
        frame: Locator,
    },
    /// `Z`: the kind 32 of one reference.
    PathReference {
        action: u8,
        project: Id,
        frame: Locator,
    },
    Segment {
        last: u64,
        bytes: u64,
        digest: Hash,
    },
    Project {
        device: u64,
        inode: u64,
        visibility: u8,
        frame: Locator,
    },
    Name {
        frame: Locator,
    },
    Identity {
        project: Id,
    },
    Conversation {
        project: Id,
        frame: Locator,
    },
    ProjectConversation {
        conversation: Id,
    },
    Activity {
        conversation: Id,
        operation: Id,
        generation: u64,
    },
    /// `p`: a dispatched person command of a conversation, keyed by its
    /// kind 26 sequence: the kind 31 input, the job, the kind 31 expected
    /// generation and the kind 31 locator.
    PersonCommand {
        input: Id,
        job: Id,
        expected_generation: u64,
        queued: Locator,
    },
}
/// The `O` facts of a terminal operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OperationFacts {
    pub conversation: Id,
    pub generation: u64,
    pub terminal: TerminalKind,
    pub cause: Cause,
    pub response_len: u64,
    pub pieces: u32,
    pub calls: u32,
    pub accepted: Locator,
    pub terminal_frame: Locator,
}

struct Out {
    bytes: Value,
    at: usize,
}
impl Out {
    fn new() -> Self {
        Self {
            bytes: [0; VALUE_BYTES],
            at: 0,
        }
    }
    fn put(&mut self, data: &[u8]) {
        self.bytes[self.at..self.at + data.len()].copy_from_slice(data);
        self.at += data.len();
    }
    fn u64(&mut self, n: u64) {
        self.put(&n.to_be_bytes());
    }
    fn u32(&mut self, n: u32) {
        self.put(&n.to_be_bytes());
    }
    fn locator(&mut self, locator: &Locator) {
        self.put(&locator.encode());
    }
}
struct In<'a> {
    bytes: &'a [u8],
    at: usize,
}
impl<'a> In<'a> {
    fn take(&mut self, n: usize) -> &'a [u8] {
        let out = &self.bytes[self.at..self.at + n];
        self.at += n;
        out
    }
    fn byte(&mut self) -> u8 {
        self.take(1)[0]
    }
    fn u64(&mut self) -> u64 {
        u64::from_be_bytes(self.take(8).try_into().expect("8 bytes"))
    }
    fn u32(&mut self) -> u32 {
        u32::from_be_bytes(self.take(4).try_into().expect("4 bytes"))
    }
    fn u16(&mut self) -> u16 {
        u16::from_be_bytes(self.take(2).try_into().expect("2 bytes"))
    }
    fn id(&mut self) -> Id {
        self.take(16).try_into().expect("16 bytes")
    }
    fn hash(&mut self) -> Hash {
        self.take(32).try_into().expect("32 bytes")
    }
    fn locator(&mut self) -> Locator {
        Locator::decode(self.take(24))
    }
    /// Unused value bytes must be zero.
    fn finish(&self) -> Result<()> {
        if self.bytes[self.at..].iter().any(|b| *b != 0) {
            return Err(ReplayError::Corrupt);
        }
        Ok(())
    }
}

impl Entry {
    pub fn tag(&self) -> Tag {
        match self {
            Self::Request { .. } => Tag::Request,
            Self::Generated { .. } => Tag::Generated,
            Self::Operation(_) => Tag::Operation,
            Self::Piece { .. } => Tag::Piece,
            Self::Thinking { .. } => Tag::Thinking,
            Self::Call { .. } => Tag::Call,
            Self::Turn { .. } => Tag::Turn,
            Self::Input { .. } => Tag::Input,
            Self::JobPayload { .. } => Tag::Payload,
            Self::Job { .. } => Tag::Job,
            Self::ConversationJob { .. } => Tag::ConversationJob,
            Self::ProjectJob { .. } => Tag::ProjectJob,
            Self::AllJobs { .. } => Tag::AllJobs,
            Self::Settlement { .. } => Tag::Settlement,
            Self::Retention { .. } => Tag::Retention,
            Self::PathChange { .. } => Tag::PathChange,
            Self::PathAck { .. } => Tag::PathAck,
            Self::PathApproval { .. } => Tag::PathApproval,
            Self::PathReference { .. } => Tag::PathReference,
            Self::Segment { .. } => Tag::Segment,
            Self::Project { .. } => Tag::Project,
            Self::Name { .. } => Tag::Name,
            Self::Identity { .. } => Tag::Identity,
            Self::Conversation { .. } => Tag::Conversation,
            Self::ProjectConversation { .. } => Tag::ProjectConversation,
            Self::Activity { .. } => Tag::Activity,
            Self::PersonCommand { .. } => Tag::PersonCommand,
        }
    }
    pub fn encode(&self) -> Value {
        let mut out = Out::new();
        match self {
            Self::Request {
                kind,
                digest,
                frame,
            } => {
                out.put(&kind.to_be_bytes());
                out.put(digest);
                out.locator(frame);
            }
            Self::Generated { entity, frame } => {
                out.put(&[*entity as u8]);
                out.locator(frame);
            }
            Self::Operation(o) => {
                out.put(&o.conversation);
                out.u64(o.generation);
                out.put(&[o.terminal as u8, o.cause as u8]);
                out.u64(o.response_len);
                out.u32(o.pieces);
                out.u32(o.calls);
                out.locator(&o.accepted);
                out.locator(&o.terminal_frame);
            }
            Self::Piece { kept, frame } | Self::Thinking { kept, frame } => {
                out.u64(*kept);
                out.locator(frame);
            }
            Self::Call {
                decision,
                intent,
                result,
            } => {
                out.put(&[*decision]);
                out.locator(intent);
                out.locator(result);
            }
            Self::Turn {
                operation,
                terminal,
                accepted,
                completed,
                notice_through,
            } => {
                out.put(operation);
                out.put(&[*terminal as u8]);
                out.locator(accepted);
                out.u64(*completed);
                out.u64(*notice_through);
            }
            Self::Input {
                status,
                queued,
                promoted,
                operation,
            } => {
                out.put(&[*status]);
                out.locator(queued);
                out.locator(promoted);
                out.put(operation);
            }
            Self::JobPayload {
                job,
                intent,
                settled,
                last_use,
            } => {
                out.put(&[2]);
                out.put(job);
                out.locator(intent);
                out.locator(settled);
                out.u64(*last_use);
            }
            Self::Job {
                conversation,
                origin,
                intent,
                cancel,
                settled,
            } => {
                out.put(conversation);
                out.put(&[*origin]);
                out.locator(intent);
                out.locator(cancel);
                out.locator(settled);
            }
            Self::ConversationJob { job } | Self::Settlement { job } => out.put(job),
            Self::ProjectJob { job, conversation } => {
                out.put(job);
                out.put(conversation);
            }
            Self::AllJobs { job, project } => {
                out.put(job);
                out.put(project);
            }
            Self::Retention {
                action,
                author,
                frame,
            } => {
                out.put(&[*action, *author]);
                out.locator(frame);
            }
            Self::PathChange { job, number, frame } => {
                out.put(job);
                out.u64(*number);
                out.locator(frame);
            }
            Self::PathAck {
                input,
                through,
                covered,
                frame,
            } => {
                out.put(input);
                out.u64(*through);
                out.u64(*covered);
                out.locator(frame);
            }
            Self::PathApproval {
                action,
                path_key,
                digest,
                frame,
            } => {
                out.put(&[*action]);
                out.put(path_key);
                out.put(digest);
                out.locator(frame);
            }
            Self::PathReference {
                action,
                project,
                frame,
            } => {
                out.put(&[*action]);
                out.put(project);
                out.locator(frame);
            }
            Self::Segment {
                last,
                bytes,
                digest,
            } => {
                out.u64(*last);
                out.u64(*bytes);
                out.put(digest);
            }
            Self::Project {
                device,
                inode,
                visibility,
                frame,
            } => {
                out.u64(*device);
                out.u64(*inode);
                out.put(&[*visibility]);
                out.locator(frame);
            }
            Self::Name { frame } => out.locator(frame),
            Self::Identity { project } => out.put(project),
            Self::Conversation { project, frame } => {
                out.put(project);
                out.locator(frame);
            }
            Self::ProjectConversation { conversation } => out.put(conversation),
            Self::Activity {
                conversation,
                operation,
                generation,
            } => {
                out.put(conversation);
                out.put(operation);
                out.u64(*generation);
            }
            Self::PersonCommand {
                input,
                job,
                expected_generation,
                queued,
            } => {
                out.put(input);
                out.put(job);
                out.u64(*expected_generation);
                out.locator(queued);
            }
        }
        out.bytes
    }
    pub fn decode(tag: Tag, value: &[u8]) -> Result<Self> {
        if value.len() != VALUE_BYTES {
            return Err(ReplayError::Corrupt);
        }
        let mut r = In {
            bytes: value,
            at: 0,
        };
        let entry = match tag {
            Tag::Request => Self::Request {
                kind: r.u16(),
                digest: r.hash(),
                frame: r.locator(),
            },
            Tag::Generated => Self::Generated {
                entity: Generated::from_code(r.byte()).ok_or(ReplayError::Corrupt)?,
                frame: r.locator(),
            },
            Tag::Operation => Self::Operation(OperationFacts {
                conversation: r.id(),
                generation: r.u64(),
                terminal: TerminalKind::from_code(r.byte()).ok_or(ReplayError::Corrupt)?,
                cause: Cause::from_code(r.byte()).ok_or(ReplayError::Corrupt)?,
                response_len: r.u64(),
                pieces: r.u32(),
                calls: r.u32(),
                accepted: r.locator(),
                terminal_frame: r.locator(),
            }),
            Tag::Piece => Self::Piece {
                kept: r.u64(),
                frame: r.locator(),
            },
            Tag::Thinking => Self::Thinking {
                kept: r.u64(),
                frame: r.locator(),
            },
            Tag::Call => Self::Call {
                decision: r.byte(),
                intent: r.locator(),
                result: r.locator(),
            },
            Tag::Turn => Self::Turn {
                operation: r.id(),
                terminal: TerminalKind::from_code(r.byte()).ok_or(ReplayError::Corrupt)?,
                accepted: r.locator(),
                completed: r.u64(),
                notice_through: r.u64(),
            },
            Tag::Input => Self::Input {
                status: r.byte(),
                queued: r.locator(),
                promoted: r.locator(),
                operation: r.id(),
            },
            Tag::Payload => match r.byte() {
                2 => Self::JobPayload {
                    job: r.id(),
                    intent: r.locator(),
                    settled: r.locator(),
                    last_use: r.u64(),
                },
                _ => return Err(ReplayError::Corrupt),
            },
            Tag::Job => Self::Job {
                conversation: r.id(),
                origin: r.byte(),
                intent: r.locator(),
                cancel: r.locator(),
                settled: r.locator(),
            },
            Tag::ConversationJob => Self::ConversationJob { job: r.id() },
            Tag::ProjectJob => Self::ProjectJob {
                job: r.id(),
                conversation: r.id(),
            },
            Tag::AllJobs => Self::AllJobs {
                job: r.id(),
                project: r.id(),
            },
            Tag::Settlement => Self::Settlement { job: r.id() },
            Tag::Retention => Self::Retention {
                action: r.byte(),
                author: r.byte(),
                frame: r.locator(),
            },
            Tag::PathChange => Self::PathChange {
                job: r.id(),
                number: r.u64(),
                frame: r.locator(),
            },
            Tag::PathAck => Self::PathAck {
                input: r.id(),
                through: r.u64(),
                covered: r.u64(),
                frame: r.locator(),
            },
            Tag::PathApproval => Self::PathApproval {
                action: r.byte(),
                path_key: r.hash(),
                digest: r.hash(),
                frame: r.locator(),
            },
            Tag::PathReference => Self::PathReference {
                action: r.byte(),
                project: r.id(),
                frame: r.locator(),
            },
            Tag::Segment => Self::Segment {
                last: r.u64(),
                bytes: r.u64(),
                digest: r.hash(),
            },
            Tag::Project => Self::Project {
                device: r.u64(),
                inode: r.u64(),
                visibility: r.byte(),
                frame: r.locator(),
            },
            Tag::Name => Self::Name { frame: r.locator() },
            Tag::Identity => Self::Identity { project: r.id() },
            Tag::Conversation => Self::Conversation {
                project: r.id(),
                frame: r.locator(),
            },
            Tag::ProjectConversation => Self::ProjectConversation {
                conversation: r.id(),
            },
            Tag::Activity => Self::Activity {
                conversation: r.id(),
                operation: r.id(),
                generation: r.u64(),
            },
            Tag::PersonCommand => Self::PersonCommand {
                input: r.id(),
                job: r.id(),
                expected_generation: r.u64(),
                queued: r.locator(),
            },
        };
        r.finish()?;
        Ok(entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn locator(n: u64) -> Locator {
        Locator {
            sequence: n,
            segment: 1,
            offset: n as u32 * 10,
            length: 200,
        }
    }
    #[test]
    fn keys_order_as_their_encoding_and_reject_padding() {
        let keys = [
            Key::new(Tag::Activity, [9; 16], 7),
            Key::new(Tag::PathChange, [0; 16], 0),
            Key::new(Tag::Call, [1; 16], 1),
            Key::new(Tag::Call, [1; 16], 2),
            Key::new(Tag::Call, [2; 16], 0),
            Key::new(Tag::Identity, [0; 16], 0),
            Key::new(Tag::Retention, [0; 16], 0),
            Key::new(Tag::PathAck, [0; 16], 0),
            Key::new(Tag::ProjectConversation, [0; 16], 0),
            Key::new(Tag::ConversationJob, [0; 16], 0),
            Key::new(Tag::ProjectJob, [0; 16], 0),
            Key::new(Tag::Name, [0; 16], 0),
            Key::new(Tag::PathApproval, [0xff; 16], u64::MAX),
            Key::new(Tag::Turn, [0; 16], 0),
            Key::new(Tag::Settlement, [0; 16], 0),
            Key::new(Tag::Conversation, [0xff; 16], 0),
            Key::new(Tag::Job, [0; 16], 0),
            Key::new(Tag::Payload, [0; 16], 0),
            Key::new(Tag::AllJobs, [0; 16], 0),
            Key::new(Tag::PathReference, [0; 16], 0),
            Key::new(Tag::PersonCommand, [0; 16], 0),
            Key::new(Tag::PersonCommand, [0; 16], 1),
        ];
        // Tag::ALL is in byte order with no gaps in the encoding check.
        for pair in Tag::ALL.windows(2) {
            assert!((pair[0] as u8) < (pair[1] as u8));
        }

        for pair in keys.windows(2) {
            assert!(pair[0] < pair[1]);
            assert!(pair[0].encode() < pair[1].encode());
        }
        for key in keys {
            assert_eq!(Key::decode(&key.encode()).unwrap(), key);
        }
        let mut bad = keys[0].encode();
        bad[31] = 1;
        assert_eq!(Key::decode(&bad), Err(ReplayError::Corrupt));
        for unknown in [0, b'@', b'[', b'a', b'o', b'q', b'z', 0xff] {
            let mut bad = keys[0].encode();
            bad[0] = unknown;
            assert_eq!(Key::decode(&bad), Err(ReplayError::Corrupt), "{unknown}");
        }
    }
    #[test]
    fn every_value_round_trips_within_96_bytes() {
        let entries = [
            Entry::Request {
                kind: 21,
                digest: [3; 32],
                frame: locator(4),
            },
            Entry::Generated {
                entity: Generated::Dispatch,
                frame: locator(5),
            },
            Entry::Operation(OperationFacts {
                conversation: [1; 16],
                generation: 3,
                terminal: TerminalKind::Cancelled,
                cause: Cause::Steering,
                response_len: u64::MAX,
                pieces: 65_536,
                calls: 201,
                accepted: locator(6),
                terminal_frame: locator(7),
            }),
            Entry::Piece {
                kept: 61_440,
                frame: locator(8),
            },
            Entry::Thinking {
                kept: 61_440,
                frame: locator(18),
            },
            Entry::Call {
                decision: 1,
                intent: locator(9),
                result: locator(10),
            },
            Entry::Turn {
                operation: [2; 16],
                terminal: TerminalKind::Complete,
                accepted: locator(11),
                completed: 9,
                notice_through: u64::MAX,
            },
            Entry::Input {
                status: 6,
                queued: locator(12),
                promoted: Locator::NONE,
                operation: [0; 16],
            },
            Entry::JobPayload {
                job: [4; 16],
                intent: locator(19),
                settled: locator(20),
                last_use: 20,
            },
            Entry::Job {
                conversation: [5; 16],
                origin: 1,
                intent: locator(19),
                cancel: Locator::NONE,
                settled: locator(20),
            },
            Entry::ConversationJob { job: [11; 16] },
            Entry::ProjectJob {
                job: [12; 16],
                conversation: [13; 16],
            },
            Entry::AllJobs {
                job: [14; 16],
                project: [15; 16],
            },
            Entry::Settlement { job: [16; 16] },
            Entry::Retention {
                action: 4,
                author: 2,
                frame: locator(21),
            },
            Entry::PathChange {
                job: [17; 16],
                number: u64::MAX,
                frame: locator(22),
            },
            Entry::PathAck {
                input: [18; 16],
                through: u64::MAX,
                covered: u64::MAX,
                frame: locator(23),
            },
            Entry::PathApproval {
                action: 3,
                path_key: [0xff; 32],
                digest: [0xfe; 32],
                frame: locator(24),
            },
            Entry::PathReference {
                action: 4,
                project: [19; 16],
                frame: locator(25),
            },
            Entry::Segment {
                last: 32_768,
                bytes: 8 << 20,
                digest: [5; 32],
            },
            Entry::Project {
                device: 1,
                inode: 2,
                visibility: 1,
                frame: locator(15),
            },
            Entry::Name { frame: locator(16) },
            Entry::Identity { project: [6; 16] },
            Entry::Conversation {
                project: [7; 16],
                frame: locator(17),
            },
            Entry::ProjectConversation {
                conversation: [8; 16],
            },
            Entry::Activity {
                conversation: [9; 16],
                operation: [10; 16],
                generation: 4,
            },
            Entry::PersonCommand {
                input: [20; 16],
                job: [21; 16],
                expected_generation: u64::MAX,
                queued: locator(26),
            },
        ];
        // `X` form byte: 2 a job; form 1 (a J1 call) is retired and, like
        // any other, Corrupt.
        let job_payload = &entries[8];
        assert_eq!(job_payload.encode()[0], 2);
        for other in [0, 1, 3, 0xff] {
            let mut value = job_payload.encode();
            value[0] = other;
            assert_eq!(
                Entry::decode(Tag::Payload, &value),
                Err(ReplayError::Corrupt)
            );
        }
        for entry in entries {
            let value = entry.encode();
            assert_eq!(Entry::decode(entry.tag(), &value).unwrap(), entry);
            let mut padded = value;
            padded[VALUE_BYTES - 1] = 1;
            assert_eq!(
                Entry::decode(entry.tag(), &padded),
                Err(ReplayError::Corrupt)
            );
        }
    }
}
