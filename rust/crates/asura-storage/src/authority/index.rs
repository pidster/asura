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
    /// Tool call: operation, ordinal.
    Call = b'C',
    /// Project identity: device and inode, 0.
    Identity = b'D',
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
    /// Project name: project, name revision.
    Name = b'N',
    /// Terminal operation.
    Operation = b'O',
    /// Response piece: operation, start offset.
    Piece = b'P',
    /// Request outcome.
    Request = b'R',
    /// Sealed segment file: first sequence.
    Segment = b'S',
    /// Turn: conversation, generation.
    Turn = b'T',
    /// Conversation.
    Conversation = b'V',
    /// Retained exec_cmd payload.
    Payload = b'X',
}
impl Tag {
    pub const ALL: [Tag; 16] = [
        Tag::Activity,
        Tag::Call,
        Tag::Identity,
        Tag::Generated,
        Tag::Thinking,
        Tag::Input,
        Tag::Project,
        Tag::ProjectConversation,
        Tag::Name,
        Tag::Operation,
        Tag::Piece,
        Tag::Request,
        Tag::Segment,
        Tag::Turn,
        Tag::Conversation,
        Tag::Payload,
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
    Call {
        decision: u8,
        intent: Locator,
        settled: Locator,
        result: Locator,
    },
    Turn {
        operation: Id,
        terminal: TerminalKind,
        accepted: Locator,
        completed: u64,
    },
    Input {
        status: u8,
        queued: Locator,
        promoted: Locator,
        operation: Id,
    },
    Payload {
        operation: Id,
        generation: u64,
        ordinal: u32,
        intent: Locator,
        settled: Locator,
        last_use: u64,
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
            Self::Payload { .. } => Tag::Payload,
            Self::Segment { .. } => Tag::Segment,
            Self::Project { .. } => Tag::Project,
            Self::Name { .. } => Tag::Name,
            Self::Identity { .. } => Tag::Identity,
            Self::Conversation { .. } => Tag::Conversation,
            Self::ProjectConversation { .. } => Tag::ProjectConversation,
            Self::Activity { .. } => Tag::Activity,
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
                settled,
                result,
            } => {
                out.put(&[*decision]);
                out.locator(intent);
                out.locator(settled);
                out.locator(result);
            }
            Self::Turn {
                operation,
                terminal,
                accepted,
                completed,
            } => {
                out.put(operation);
                out.put(&[*terminal as u8]);
                out.locator(accepted);
                out.u64(*completed);
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
            Self::Payload {
                operation,
                generation,
                ordinal,
                intent,
                settled,
                last_use,
            } => {
                out.put(operation);
                out.u64(*generation);
                out.u32(*ordinal);
                out.locator(intent);
                out.locator(settled);
                out.u64(*last_use);
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
                settled: r.locator(),
                result: r.locator(),
            },
            Tag::Turn => Self::Turn {
                operation: r.id(),
                terminal: TerminalKind::from_code(r.byte()).ok_or(ReplayError::Corrupt)?,
                accepted: r.locator(),
                completed: r.u64(),
            },
            Tag::Input => Self::Input {
                status: r.byte(),
                queued: r.locator(),
                promoted: r.locator(),
                operation: r.id(),
            },
            Tag::Payload => Self::Payload {
                operation: r.id(),
                generation: r.u64(),
                ordinal: r.u32(),
                intent: r.locator(),
                settled: r.locator(),
                last_use: r.u64(),
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
            Key::new(Tag::Call, [1; 16], 1),
            Key::new(Tag::Call, [1; 16], 2),
            Key::new(Tag::Call, [2; 16], 0),
            Key::new(Tag::Payload, [0; 16], 0),
        ];
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
        let mut bad = keys[0].encode();
        bad[0] = b'Z';
        assert_eq!(Key::decode(&bad), Err(ReplayError::Corrupt));
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
                settled: Locator::NONE,
                result: locator(10),
            },
            Entry::Turn {
                operation: [2; 16],
                terminal: TerminalKind::Complete,
                accepted: locator(11),
                completed: 9,
            },
            Entry::Input {
                status: 6,
                queued: locator(12),
                promoted: Locator::NONE,
                operation: [0; 16],
            },
            Entry::Payload {
                operation: [3; 16],
                generation: 2,
                ordinal: 200,
                intent: locator(13),
                settled: locator(14),
                last_use: 14,
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
        ];
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
