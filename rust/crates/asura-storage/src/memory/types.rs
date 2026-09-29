use sha2::{Digest, Sha256};

pub const MAX_BODY: usize = 16 * 1024;
pub type Result<T> = std::result::Result<T, Error>;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    InvalidInput,
    LimitExceeded,
    UnsafePath,
    MissingDatabase,
    BindingMismatch,
    NotFound,
    IdempotencyConflict,
    Busy,
    Cancelled,
    Timeout,
    OutcomeUnconfirmed,
    Unavailable,
    InvalidRecord,
    Closed,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "memory_{self:?}")
    }
}
impl std::error::Error for Error {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Id([u8; 16]);
impl Id {
    pub fn new(bytes: [u8; 16]) -> Result<Self> {
        if bytes == [0; 16] {
            Err(Error::InvalidInput)
        } else {
            Ok(Self(bytes))
        }
    }
    pub fn bytes(self) -> [u8; 16] {
        self.0
    }
    pub(crate) fn hex(self) -> String {
        hex(&self.0)
    }
    pub(crate) fn parse(text: &str) -> Result<Self> {
        let bytes = parse_hex::<16>(text)?;
        Self::new(bytes).map_err(|_| Error::InvalidRecord)
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Binding {
    pub installation_id: Id,
    pub graph_id: Id,
    pub init_operation_id: Id,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PutNote {
    pub binding: Binding,
    pub context_id: Id,
    pub object_id: Id,
    pub version_id: Id,
    pub operation_id: Id,
    pub body: String,
    /// Exact source version and caller-supplied edge identity.
    pub source: Option<(Id, Id)>,
}
impl PutNote {
    pub fn command_digest(&self) -> Result<[u8; 32]> {
        if self.body.len() > MAX_BODY {
            return Err(Error::LimitExceeded);
        }
        let mut hash = Sha256::new();
        hash.update(b"asura-memory-put-v1");
        for id in [
            self.binding.installation_id,
            self.binding.graph_id,
            self.binding.init_operation_id,
            self.context_id,
            self.object_id,
            self.version_id,
            self.operation_id,
        ] {
            hash.update(16u32.to_be_bytes());
            hash.update(id.0);
        }
        hash.update((self.body.len() as u32).to_be_bytes());
        hash.update(self.body.as_bytes());
        hash.update([u8::from(self.source.is_some())]);
        if let Some((source, edge)) = self.source {
            hash.update(source.0);
            hash.update(edge.0);
        }
        Ok(hash.finalize().into())
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Note {
    pub binding: Binding,
    pub context_id: Id,
    pub object_id: Id,
    pub version_id: Id,
    pub operation_id: Id,
    pub body: String,
    pub body_sha256: [u8; 32],
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Receipt {
    pub operation_id: Id,
    pub command_sha256: [u8; 32],
    pub version_id: Id,
    pub edge_id: Option<Id>,
}
pub(crate) fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}
pub(crate) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 15) as usize] as char);
    }
    output
}
pub(crate) fn parse_hex<const N: usize>(text: &str) -> Result<[u8; N]> {
    if text.len() != N * 2 {
        return Err(Error::InvalidRecord);
    }
    let mut result = [0; N];
    fn nibble(byte: u8) -> Result<u8> {
        match byte {
            b'0'..=b'9' => Ok(byte - b'0'),
            b'a'..=b'f' => Ok(byte - b'a' + 10),
            _ => Err(Error::InvalidRecord),
        }
    }
    for (at, pair) in text.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        result[at] = nibble(pair[0])? * 16 + nibble(pair[1])?;
    }
    Ok(result)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn closed_id_encoding_rejects_zero_uppercase_and_wrong_length() {
        assert!(Id::new([0; 16]).is_err());
        assert!(Id::parse(&"A".repeat(32)).is_err());
        assert!(Id::parse("01").is_err());
        let id = Id::new([0xab; 16]).unwrap();
        assert_eq!(Id::parse(&id.hex()).unwrap(), id);
    }
    #[test]
    fn digest_includes_scope_source_and_exact_body() {
        let id = |n| Id::new([n; 16]).unwrap();
        let mut note = PutNote {
            binding: Binding {
                installation_id: id(1),
                graph_id: id(2),
                init_operation_id: id(3),
            },
            context_id: id(4),
            object_id: id(5),
            version_id: id(6),
            operation_id: id(7),
            body: "literal".into(),
            source: None,
        };
        let original = note.command_digest().unwrap();
        note.body.push('\n');
        assert_ne!(original, note.command_digest().unwrap());
        note.body = "literal".into();
        note.source = Some((id(8), id(9)));
        assert_ne!(original, note.command_digest().unwrap());
        note.body = "x".repeat(MAX_BODY + 1);
        assert_eq!(note.command_digest(), Err(Error::LimitExceeded));
    }
}

/// A live keyset page; IDs order immutable versions, not creation time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListNotes {
    pub binding: Binding,
    pub context_id: Id,
    pub after: Option<Id>,
    pub limit: u8,
}
impl ListNotes {
    pub fn validate(&self) -> Result<()> {
        if (1..=32).contains(&self.limit) {
            Ok(())
        } else {
            Err(Error::InvalidInput)
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoteSummary {
    pub object_id: Id,
    pub version_id: Id,
    pub operation_id: Id,
    pub body_sha256: [u8; 32],
    pub preview: String,
}
impl From<Note> for NoteSummary {
    fn from(note: Note) -> Self {
        let mut end = note.body.len().min(256);
        while !note.body.is_char_boundary(end) {
            end -= 1;
        }
        Self {
            object_id: note.object_id,
            version_id: note.version_id,
            operation_id: note.operation_id,
            body_sha256: note.body_sha256,
            preview: note.body[..end].to_owned(),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotePage {
    pub notes: Vec<NoteSummary>,
    pub next: Option<Id>,
}

/// Read selection; the authority owner supplies binding and project scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadNotes {
    Get(Id),
    Sources(Id),
    List { after: Option<Id>, limit: u8 },
}
impl ReadNotes {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::List { limit, .. } if !(1..=32).contains(limit) => Err(Error::InvalidInput),
            _ => Ok(()),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadResult {
    Note(Note),
    Sources(Vec<Note>),
    Page(NotePage),
}

#[cfg(test)]
mod discovery_tests {
    use super::*;
    #[test]
    fn list_limits_and_preview_boundaries_are_exact() {
        for limit in 0..=255 {
            assert_eq!(
                ReadNotes::List { after: None, limit }.validate().is_ok(),
                (1..=32).contains(&limit)
            );
        }
        let id = Id::new([1; 16]).unwrap();
        for body in [
            String::new(),
            "a".repeat(256),
            "a".repeat(255) + "€",
            "😀".repeat(65),
        ] {
            let expected = body
                .char_indices()
                .map(|(at, _)| at)
                .chain(std::iter::once(body.len()))
                .filter(|at| *at <= 256)
                .max()
                .unwrap();
            let summary = NoteSummary::from(Note {
                binding: Binding {
                    installation_id: id,
                    graph_id: id,
                    init_operation_id: id,
                },
                context_id: id,
                object_id: id,
                version_id: id,
                operation_id: id,
                body_sha256: digest(body.as_bytes()),
                body: body.clone(),
            });
            assert_eq!(summary.preview, body[..expected]);
            assert_eq!(summary.body_sha256, digest(body.as_bytes()));
        }
    }
}
