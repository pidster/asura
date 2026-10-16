//! Payload identities (J1). Pure computation.
use crate::authority::conversation::Id;
use sha2::{Digest, Sha256};

/// The object a payload holds. The kind makes the ID generic for later
/// typed results (SL6) and the composer's page requests (CC2, G20).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
#[repr(u8)]
pub enum ObjectKind {
    Stdout = 1,
    Stderr = 2,
    /// Reserved for typed tool results (SL6).
    Result = 3,
}

/// The first 16 bytes of SHA-256 over ASCII `asura.payload`, one zero byte,
/// the installation ID, the operation ID, the generation (8 bytes, big
/// endian), the ordinal (4 bytes, big endian) and the object kind byte. An
/// all-zero result becomes 1 in its final byte.
pub fn payload_id(
    installation: Id,
    operation: Id,
    generation: u64,
    ordinal: u32,
    kind: ObjectKind,
) -> Id {
    let mut hash = Sha256::new();
    hash.update(b"asura.payload\0");
    hash.update(installation);
    hash.update(operation);
    hash.update(generation.to_be_bytes());
    hash.update(ordinal.to_be_bytes());
    hash.update([kind as u8]);
    let digest = hash.finalize();
    let mut id = [0; 16];
    id.copy_from_slice(&digest[..16]);
    if id == [0; 16] {
        id[15] = 1;
    }
    id
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn payload_ids_depend_on_every_input_and_kind() {
        let base = payload_id([1; 16], [2; 16], 3, 4, ObjectKind::Stdout);
        assert_ne!(base, [0; 16]);
        assert_eq!(base, payload_id([1; 16], [2; 16], 3, 4, ObjectKind::Stdout));
        for other in [
            payload_id([9; 16], [2; 16], 3, 4, ObjectKind::Stdout),
            payload_id([1; 16], [9; 16], 3, 4, ObjectKind::Stdout),
            payload_id([1; 16], [2; 16], 9, 4, ObjectKind::Stdout),
            payload_id([1; 16], [2; 16], 3, 9, ObjectKind::Stdout),
            payload_id([1; 16], [2; 16], 3, 4, ObjectKind::Stderr),
            payload_id([1; 16], [2; 16], 3, 4, ObjectKind::Result),
        ] {
            assert_ne!(base, other);
        }
    }
}
