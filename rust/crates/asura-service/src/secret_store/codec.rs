//! The secret item payload codec (`docs/designs/secret-store.md`, payload).
//!
//! This codec is the one owner of the payload bound: it rejects any item data
//! over [`MAX_ITEM_BYTES`] before it parses a byte, so the worker replies
//! `mismatch` for an oversize item. All integers are big-endian. Unknown
//! versions, unknown flag bits, trailing bytes and every bound violation are
//! rejected.
use asura_platform::keychain::MAX_ITEM_BYTES;
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

/// Payload format version.
pub const VERSION: u8 = 1;
/// Numbered secret variables per item; numbers are 1 to this value.
pub const MAX_VARIABLES: usize = 16;
/// Secret variable values in total, as secret detection requires.
pub const MAX_VARIABLE_BYTES: usize = 8_192;
/// The whole-command value.
pub const MAX_WHOLE_COMMAND_BYTES: usize = 8_192;
/// Bytes of the binding digest and of the value digest key.
pub const DIGEST_BYTES: usize = 32;

const FLAG_WHOLE_COMMAND: u8 = 0x1;
const BINDING_DOMAIN: &[u8] = b"asura.secret-item";
const VALUE_DOMAIN: &[u8] = b"asura.secret-values";

/// One numbered secret variable of a rewritten person command.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretValue {
    /// The variable number, 1 to 16.
    pub number: u8,
    /// The value, 1 byte or more.
    pub value: Zeroizing<Vec<u8>>,
}

/// All values of one person command input. Every buffer zeroizes on drop.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Secrets {
    /// Numbered secret variables in ascending number order. Empty when
    /// `whole_command` is present: the whole-command variable is used alone.
    pub variables: Vec<SecretValue>,
    /// The raw command for the whole-command variable, 1 to 8,192 bytes.
    pub whole_command: Option<Zeroizing<Vec<u8>>>,
}

impl std::fmt::Debug for Secrets {
    /// Counts only: a value or length never reaches a log.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Secrets")
            .field("variables", &self.variables.len())
            .field("whole_command", &self.whole_command.is_some())
            .finish()
    }
}

/// Why a payload was rejected. Carries no value, name or size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodecError {
    /// The data is over [`MAX_ITEM_BYTES`].
    Oversize,
    /// The data ended inside a field.
    Truncated,
    /// Bytes follow the value digest key.
    Trailing,
    /// The version is not [`VERSION`].
    Version,
    /// The binding digest is not the expected one.
    Binding,
    /// A count, length, number, flag or total broke a rule.
    Bound,
}

/// The binding digest: SHA-256 over ASCII `asura.secret-item`, one zero
/// byte, then the installation, project, conversation and input IDs and the
/// kind 31 request digest. Identifiers only; never a value.
pub fn binding(
    installation: &[u8; 16],
    project: &[u8; 16],
    conversation: &[u8; 16],
    input: &[u8; 16],
    request_digest: &[u8; DIGEST_BYTES],
) -> [u8; DIGEST_BYTES] {
    let mut hash = Sha256::new();
    hash.update(BINDING_DOMAIN);
    hash.update([0]);
    hash.update(installation);
    hash.update(project);
    hash.update(conversation);
    hash.update(input);
    hash.update(request_digest);
    hash.finalize().into()
}

/// Checks every bound of `secrets` in one place, for encode and decode.
pub(crate) fn validate(secrets: &Secrets) -> Result<(), CodecError> {
    let variables = &secrets.variables;
    if variables.len() > MAX_VARIABLES || (secrets.whole_command.is_some() && !variables.is_empty())
    {
        return Err(CodecError::Bound);
    }
    let mut previous = 0u8;
    let mut total = 0usize;
    for variable in variables {
        if variable.number <= previous
            || usize::from(variable.number) > MAX_VARIABLES
            || variable.value.is_empty()
        {
            return Err(CodecError::Bound);
        }
        previous = variable.number;
        total += variable.value.len();
    }
    if total > MAX_VARIABLE_BYTES {
        return Err(CodecError::Bound);
    }
    if let Some(command) = &secrets.whole_command
        && (command.is_empty() || command.len() > MAX_WHOLE_COMMAND_BYTES)
    {
        return Err(CodecError::Bound);
    }
    Ok(())
}

/// The canonical value section: flags, variables and whole-command value.
/// Equal secrets give equal bytes. Flag bit 1, the launch environment that
/// ENV1 removed, is never set.
fn body(secrets: &Secrets, out: &mut Vec<u8>) {
    let mut flags = 0u8;
    if secrets.whole_command.is_some() {
        flags |= FLAG_WHOLE_COMMAND;
    }
    out.push(flags);
    // Bounds were validated, so every cast below is lossless.
    out.push(secrets.variables.len() as u8);
    for variable in &secrets.variables {
        out.push(variable.number);
        out.extend_from_slice(&(variable.value.len() as u16).to_be_bytes());
        out.extend_from_slice(&variable.value);
    }
    if let Some(command) = &secrets.whole_command {
        out.extend_from_slice(&(command.len() as u16).to_be_bytes());
        out.extend_from_slice(command);
    }
}

/// Encodes one item payload. The caller supplies the random value digest key.
pub(crate) fn encode(
    binding: &[u8; DIGEST_BYTES],
    secrets: &Secrets,
    key: &[u8; DIGEST_BYTES],
) -> Result<Zeroizing<Vec<u8>>, CodecError> {
    validate(secrets)?;
    let mut out = Zeroizing::new(Vec::with_capacity(MAX_ITEM_BYTES));
    out.push(VERSION);
    out.extend_from_slice(binding);
    body(secrets, &mut out);
    out.extend_from_slice(key);
    debug_assert!(out.len() <= MAX_ITEM_BYTES);
    Ok(out)
}

/// A decoded payload: the values and the item's value digest key.
pub(crate) struct Decoded {
    pub secrets: Secrets,
    pub key: Zeroizing<[u8; DIGEST_BYTES]>,
}

struct Reader<'a> {
    data: &'a [u8],
}
impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], CodecError> {
        if self.data.len() < n {
            return Err(CodecError::Truncated);
        }
        let (head, tail) = self.data.split_at(n);
        self.data = tail;
        Ok(head)
    }
    fn u8(&mut self) -> Result<u8, CodecError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<usize, CodecError> {
        let bytes = self.take(2)?;
        Ok(usize::from(u16::from_be_bytes([bytes[0], bytes[1]])))
    }
    /// A length-prefixed field no longer than `max`, checked before the copy.
    fn bytes(&mut self, len: usize, max: usize) -> Result<Zeroizing<Vec<u8>>, CodecError> {
        if len > max {
            return Err(CodecError::Bound);
        }
        Ok(Zeroizing::new(self.take(len)?.to_vec()))
    }
}

/// Decodes item data read from the Keychain and checks its binding.
pub(crate) fn decode(data: &[u8], expected: &[u8; DIGEST_BYTES]) -> Result<Decoded, CodecError> {
    if data.len() > MAX_ITEM_BYTES {
        return Err(CodecError::Oversize);
    }
    let mut reader = Reader { data };
    if reader.u8()? != VERSION {
        return Err(CodecError::Version);
    }
    if reader.take(DIGEST_BYTES)? != expected {
        return Err(CodecError::Binding);
    }
    let flags = reader.u8()?;
    if flags & !FLAG_WHOLE_COMMAND != 0 {
        return Err(CodecError::Bound);
    }
    let count = usize::from(reader.u8()?);
    if count > MAX_VARIABLES {
        return Err(CodecError::Bound);
    }
    let mut secrets = Secrets::default();
    for _ in 0..count {
        let number = reader.u8()?;
        let len = reader.u16()?;
        let value = reader.bytes(len, MAX_VARIABLE_BYTES)?;
        secrets.variables.push(SecretValue { number, value });
    }
    if flags & FLAG_WHOLE_COMMAND != 0 {
        let len = reader.u16()?;
        secrets.whole_command = Some(reader.bytes(len, MAX_WHOLE_COMMAND_BYTES)?);
    }
    let mut key = Zeroizing::new([0u8; DIGEST_BYTES]);
    key.copy_from_slice(reader.take(DIGEST_BYTES)?);
    if !reader.data.is_empty() {
        return Err(CodecError::Trailing);
    }
    validate(&secrets)?;
    Ok(Decoded { secrets, key })
}

/// The keyed duplicate-retry digest of `secrets` under an item's key.
pub(crate) fn value_digest(key: &[u8; DIGEST_BYTES], secrets: &Secrets) -> [u8; DIGEST_BYTES] {
    let mut section = Zeroizing::new(Vec::new());
    body(secrets, &mut section);
    let mut hash = Sha256::new();
    hash.update(VALUE_DOMAIN);
    hash.update([0]);
    hash.update(key);
    hash.update(section.as_slice());
    hash.finalize().into()
}

/// Compares two digests without an early exit.
pub(crate) fn digests_equal(a: &[u8; DIGEST_BYTES], b: &[u8; DIGEST_BYTES]) -> bool {
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
