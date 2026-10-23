//! Payload identities (J2-E6): payload IDs keyed by job ID. Pure computation.
//! This is the only payload ID derivation; journal replay, the ledger,
//! recovery and the capture path all call it.
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
/// the installation ID, the job ID and the object kind byte. An all-zero
/// result becomes 1 in its final byte. Every job origin uses this
/// derivation; a model exec_cmd call reaches it through its model-origin
/// job ID.
pub fn payload_id(installation: Id, job: Id, kind: ObjectKind) -> Id {
    let mut hash = Sha256::new();
    hash.update(b"asura.payload\0");
    hash.update(installation);
    hash.update(job);
    hash.update([kind as u8]);
    truncated(&hash.finalize())
}

/// The stdout and stderr payload IDs of one job, in stream order.
pub fn job_payloads(installation: Id, job: Id) -> [Id; 2] {
    [ObjectKind::Stdout, ObjectKind::Stderr].map(|kind| payload_id(installation, job, kind))
}

/// The first 16 bytes of a digest; all zero becomes 1 in the final byte.
fn truncated(digest: &[u8]) -> Id {
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
    use crate::authority::conversation::{JobOrigin, job_id};

    const INSTALLATION: Id = [9; 16];

    /// J2T-04 (payload): fixed vectors for one model and one person job,
    /// computed independently of this code.
    #[test]
    fn j2t_04_payload_id_vectors_for_both_origins() {
        let model = job_id(&JobOrigin::Model {
            operation: [1; 16],
            generation: 1,
            ordinal: 1,
        });
        let person = job_id(&JobOrigin::Person { input: [7; 16] });
        let vectors: [(Id, [Id; 3]); 2] = [
            (
                model,
                [
                    [
                        0x23, 0x08, 0xfb, 0x33, 0x95, 0xb6, 0x5a, 0x87, 0xbf, 0x96, 0x8a, 0xfb,
                        0xbd, 0xa5, 0x41, 0x62,
                    ],
                    [
                        0xa3, 0x50, 0xcc, 0xc2, 0x95, 0xc3, 0x07, 0x9f, 0x57, 0xab, 0xcf, 0x7e,
                        0x99, 0x1d, 0x97, 0xf7,
                    ],
                    [
                        0x99, 0x08, 0x96, 0xdb, 0x11, 0xe8, 0x7f, 0x57, 0x3c, 0x5c, 0x1d, 0xcd,
                        0x76, 0x9d, 0xa6, 0x7a,
                    ],
                ],
            ),
            (
                person,
                [
                    [
                        0xcc, 0x59, 0xc4, 0x79, 0x80, 0x19, 0xb1, 0x82, 0x01, 0x83, 0x87, 0xee,
                        0x15, 0xb9, 0x31, 0x18,
                    ],
                    [
                        0xe2, 0x2a, 0x23, 0xab, 0x2b, 0x12, 0x54, 0x9b, 0xfe, 0xa8, 0xb6, 0x2e,
                        0xcd, 0x6f, 0xbe, 0x67,
                    ],
                    [
                        0xcd, 0x60, 0xab, 0x4a, 0xf4, 0xfe, 0x70, 0x07, 0x5a, 0x8a, 0x42, 0xae,
                        0x4e, 0x3a, 0x45, 0xee,
                    ],
                ],
            ),
        ];
        for (job, [stdout, stderr, result]) in vectors {
            assert_eq!(payload_id(INSTALLATION, job, ObjectKind::Stdout), stdout);
            assert_eq!(payload_id(INSTALLATION, job, ObjectKind::Stderr), stderr);
            assert_eq!(payload_id(INSTALLATION, job, ObjectKind::Result), result);
            assert_eq!(job_payloads(INSTALLATION, job), [stdout, stderr]);
        }
    }

    /// J2T-04 (payload): a change to each input changes the ID; the zero
    /// rule turns an all-zero prefix into 1 in the final byte.
    #[test]
    fn j2t_04_payload_ids_depend_on_every_input_and_the_zero_rule() {
        let base = payload_id([1; 16], [2; 16], ObjectKind::Stdout);
        assert_ne!(base, [0; 16]);
        assert_eq!(base, payload_id([1; 16], [2; 16], ObjectKind::Stdout));
        for other in [
            payload_id([9; 16], [2; 16], ObjectKind::Stdout),
            payload_id([1; 16], [9; 16], ObjectKind::Stdout),
            payload_id([1; 16], [2; 16], ObjectKind::Stderr),
            payload_id([1; 16], [2; 16], ObjectKind::Result),
        ] {
            assert_ne!(base, other);
        }
        let mut one = [0; 16];
        one[15] = 1;
        assert_eq!(truncated(&[0; 32]), one);
        assert_eq!(truncated(&[0xab; 32]), [0xab; 16]);
        let mut low = [0u8; 32];
        low[15] = 7;
        assert_eq!(truncated(&low)[15], 7);
    }

    /// J2T-04 (payload): a model exec_cmd call uses the job derivation
    /// through its model-origin job ID; the operation, generation and
    /// ordinal enter only through the job ID.
    #[test]
    fn j2t_04_exec_cmd_calls_use_the_job_derivation() {
        let job = crate::authority::conversation::exec_cmd_job_id([1; 16], 1, 1);
        assert_eq!(
            job,
            job_id(&JobOrigin::Model {
                operation: [1; 16],
                generation: 1,
                ordinal: 1,
            })
        );
        let [stdout, _] = job_payloads(INSTALLATION, job);
        assert_eq!(stdout[..4], [0x23, 0x08, 0xfb, 0x33]);
    }
}
