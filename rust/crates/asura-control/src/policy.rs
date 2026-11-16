//! Security policy control messages (security-policy.md, PL3 control and
//! presentation; envelope fields 77-80) and the candidate result in
//! `PolicyStatus`: presence, bounds and reply tokens. Domain errors are
//! `PolicyReply.error` tokens. Runtime-free.
use crate::{id, pb};
use pb::envelope::Body;

/// The policy text one `PolicyCandidateSubmit` carries: one control frame
/// holds it (the admission bound is 256 KiB; PL3 contract).
pub const MAX_POLICY_TEXT_BYTES: usize = 61_440;
/// Detail lines, counterexamples and widening examples of one candidate.
pub const MAX_POLICY_LINES: usize = 8;
/// Bytes of one detail line, counterexample or example.
pub const MAX_POLICY_LINE_BYTES: usize = 512;
/// Candidates that wait for admission.
pub const MAX_ADMISSION_QUEUE: u32 = 16;
/// Proof queries of one admission.
pub const MAX_POLICY_QUERIES: u32 = 4_096;
/// The user scope, the only scope a PL3 candidate may name.
pub const SCOPE_USER: u32 = 2;

/// `PolicyCandidate.state` values.
pub mod state {
    pub const QUEUED: u32 = 1;
    pub const PROVING: u32 = 2;
    pub const ACTIVATED: u32 = 3;
    pub const REJECTED: u32 = 4;
    pub const CANCELLED: u32 = 5;
}

/// `PolicyReply.error` tokens.
pub mod error {
    /// 16 candidates already wait for admission.
    pub const BUSY: &str = "busy";
    /// The scope is not the user scope (PL3).
    pub const SCOPE_INVALID: &str = "scope_invalid";
    /// The text is empty or not admissible text.
    pub const TEXT_INVALID: &str = "text_invalid";
    /// The rollback names no activated revision among the last 64.
    pub const UNKNOWN_REVISION: &str = "unknown_revision";
    /// The cancel names no queued or proving candidate.
    pub const NOT_FOUND: &str = "not_found";
    /// The policy owner is closing or not running.
    pub const UNAVAILABLE: &str = "unavailable";
    pub const ALL: [&str; 6] = [
        BUSY,
        SCOPE_INVALID,
        TEXT_INVALID,
        UNKNOWN_REVISION,
        NOT_FOUND,
        UNAVAILABLE,
    ];
}

fn scope(value: Option<u32>) -> bool {
    value.is_some_and(|s| (1..=4).contains(&s))
}
/// A reason code: `[a-z0-9_]{1,48}`.
fn reason(value: &str) -> bool {
    (1..=48).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}
/// At most 8 lines of 1 to 512 bytes with no control character.
fn lines(values: &[String]) -> bool {
    values.len() <= MAX_POLICY_LINES
        && values.iter().all(|line| {
            !line.is_empty()
                && line.len() <= MAX_POLICY_LINE_BYTES
                && !line.chars().any(char::is_control)
        })
}

/// Finished candidates in `PolicyStatus.recent`.
pub const MAX_RECENT_CANDIDATES: usize = 4;

/// `PolicyStatus.candidate`.
pub(crate) fn candidate_valid(v: &pb::PolicyCandidate) -> bool {
    let finished = v
        .state
        .is_some_and(|s| matches!(s, state::ACTIVATED | state::REJECTED | state::CANCELLED));
    id(&v.candidate_id)
        && v.state
            .is_some_and(|s| (state::QUEUED..=state::CANCELLED).contains(&s))
        && scope(v.scope)
        && v.position.is_none_or(|p| p <= MAX_ADMISSION_QUEUE)
        && v.reason.as_deref().is_none_or(reason)
        && lines(&v.details)
        && lines(&v.counterexamples)
        && lines(&v.widening_examples)
        && v.revision.is_none_or(|r| r >= 1)
        && (v.revision.is_none() || v.state == Some(state::ACTIVATED))
        && (v.reason.is_none() || finished)
        && v.queries.is_none_or(|n| n <= MAX_POLICY_QUERIES)
}

pub(crate) fn handles(body: &Body) -> bool {
    matches!(
        body,
        Body::PolicyCandidateSubmit(_)
            | Body::PolicyRollback(_)
            | Body::PolicyCancel(_)
            | Body::PolicyReply(_)
    )
}
pub(crate) fn is_request(body: &Body) -> bool {
    matches!(
        body,
        Body::PolicyCandidateSubmit(_) | Body::PolicyRollback(_) | Body::PolicyCancel(_)
    )
}

pub(crate) fn valid(body: &Body) -> bool {
    match body {
        // The text's content is the service's `text_invalid`; the codec
        // checks presence and the frame bound only.
        Body::PolicyCandidateSubmit(v) => {
            id(&v.request_id)
                && scope(v.scope)
                && v.text
                    .as_ref()
                    .is_some_and(|t| t.len() <= MAX_POLICY_TEXT_BYTES)
        }
        Body::PolicyRollback(v) => {
            id(&v.request_id) && scope(v.scope) && v.revision.is_some_and(|r| r >= 1)
        }
        Body::PolicyCancel(v) => id(&v.request_id) && id(&v.candidate_id),
        Body::PolicyReply(v) => match v.error.as_deref() {
            Some(token) => {
                error::ALL.contains(&token) && v.candidate_id.is_none() && v.position.is_none()
            }
            None => id(&v.candidate_id) && v.position.is_none_or(|p| p <= MAX_ADMISSION_QUEUE),
        },
        _ => false,
    }
}

/// A full rejected candidate at every bound, for tests.
#[cfg(test)]
pub(crate) fn tests_candidate() -> pb::PolicyCandidate {
    pb::PolicyCandidate {
        candidate_id: Some(vec![7; 16]),
        state: Some(state::REJECTED),
        scope: Some(SCOPE_USER),
        position: Some(0),
        reason: Some("proof_failed".into()),
        details: vec!["d".repeat(MAX_POLICY_LINE_BYTES); MAX_POLICY_LINES],
        counterexamples: vec!["c".repeat(MAX_POLICY_LINE_BYTES); MAX_POLICY_LINES],
        revision: None,
        widening: Some(false),
        widening_examples: vec!["w".repeat(MAX_POLICY_LINE_BYTES); MAX_POLICY_LINES],
        queries: Some(MAX_POLICY_QUERIES),
        proof_ms: Some(12),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Direction, HEADER_BYTES, MAX_FRAME_BYTES, decode_body, encode_frame, validate_semantics,
    };

    fn envelope(body: Body) -> pb::Envelope {
        pb::Envelope {
            service_epoch: Some(vec![1; 16]),
            attachment_id: Some(vec![2; 16]),
            request_counter: Some(1),
            body: Some(body),
        }
    }
    fn request(body: Body) -> bool {
        validate_semantics(&envelope(body), Direction::ClientToServer).is_ok()
    }
    fn reply(body: Body) -> bool {
        validate_semantics(&envelope(body), Direction::ServerToClient).is_ok()
    }
    fn round_trips(body: Body, direction: Direction) -> bool {
        let value = envelope(body);
        let Ok(frame) = encode_frame(&value) else {
            return false;
        };
        decode_body(&frame[HEADER_BYTES..]).is_ok_and(|decoded| decoded == value)
            && validate_semantics(&value, direction).is_ok()
    }
    fn submit(text: Option<String>, scope: Option<u32>) -> Body {
        Body::PolicyCandidateSubmit(pb::PolicyCandidateSubmit {
            request_id: Some(vec![3; 16]),
            scope,
            text,
        })
    }

    /// The largest submission fits one frame; one byte more is refused.
    #[test]
    fn submit_bounds_and_frame() {
        let max = "a".repeat(MAX_POLICY_TEXT_BYTES);
        let value = envelope(submit(Some(max.clone()), Some(SCOPE_USER)));
        let frame = encode_frame(&value).expect("frame");
        assert!(frame.len() - HEADER_BYTES <= MAX_FRAME_BYTES);
        assert!(round_trips(
            submit(Some(max), Some(SCOPE_USER)),
            Direction::ClientToServer
        ));
        // An empty text and another scope are the service's tokens.
        assert!(request(submit(Some(String::new()), Some(SCOPE_USER))));
        assert!(request(submit(Some("x".into()), Some(3))));
        for rejected in [
            submit(
                Some("a".repeat(MAX_POLICY_TEXT_BYTES + 1)),
                Some(SCOPE_USER),
            ),
            submit(None, Some(SCOPE_USER)),
            submit(Some("x".into()), None),
            submit(Some("x".into()), Some(0)),
            submit(Some("x".into()), Some(5)),
        ] {
            assert!(!request(rejected));
        }
        assert!(!request(Body::PolicyCandidateSubmit(
            pb::PolicyCandidateSubmit {
                request_id: Some(vec![0; 16]),
                scope: Some(2),
                text: Some("x".into()),
            }
        )));
        assert!(!reply(submit(Some("x".into()), Some(SCOPE_USER))));
    }

    #[test]
    fn rollback_and_cancel() {
        let rollback = |revision| {
            Body::PolicyRollback(pb::PolicyRollback {
                request_id: Some(vec![3; 16]),
                scope: Some(SCOPE_USER),
                revision,
            })
        };
        assert!(round_trips(rollback(Some(1)), Direction::ClientToServer));
        assert!(!request(rollback(Some(0))));
        assert!(!request(rollback(None)));
        assert!(!reply(rollback(Some(1))));
        let cancel = |candidate: Option<Vec<u8>>| {
            Body::PolicyCancel(pb::PolicyCancel {
                request_id: Some(vec![3; 16]),
                candidate_id: candidate,
            })
        };
        assert!(round_trips(
            cancel(Some(vec![7; 16])),
            Direction::ClientToServer
        ));
        assert!(!request(cancel(None)));
        assert!(!request(cancel(Some(vec![7; 15]))));
    }

    #[test]
    fn reply_tokens() {
        let ok = |position| {
            Body::PolicyReply(pb::PolicyReply {
                candidate_id: Some(vec![7; 16]),
                position,
                error: None,
            })
        };
        assert!(round_trips(ok(Some(0)), Direction::ServerToClient));
        assert!(round_trips(ok(Some(16)), Direction::ServerToClient));
        assert!(!reply(ok(Some(17))));
        assert!(!request(ok(Some(0))));
        for token in error::ALL {
            assert!(round_trips(
                Body::PolicyReply(pb::PolicyReply {
                    candidate_id: None,
                    position: None,
                    error: Some(token.into()),
                }),
                Direction::ServerToClient
            ));
        }
        for rejected in [
            pb::PolicyReply {
                candidate_id: None,
                position: None,
                error: Some("other".into()),
            },
            pb::PolicyReply {
                candidate_id: Some(vec![7; 16]),
                position: None,
                error: Some(error::BUSY.into()),
            },
            pb::PolicyReply {
                candidate_id: None,
                position: None,
                error: None,
            },
        ] {
            assert!(!reply(Body::PolicyReply(rejected)));
        }
    }

    fn candidate() -> pb::PolicyCandidate {
        super::tests_candidate()
    }
    #[test]
    fn candidate_bounds() {
        assert!(candidate_valid(&candidate()));
        let activated = pb::PolicyCandidate {
            state: Some(state::ACTIVATED),
            reason: None,
            revision: Some(4),
            ..candidate()
        };
        assert!(candidate_valid(&activated));
        for changed in [
            pb::PolicyCandidate {
                candidate_id: None,
                ..candidate()
            },
            pb::PolicyCandidate {
                state: Some(6),
                ..candidate()
            },
            pb::PolicyCandidate {
                position: Some(17),
                ..candidate()
            },
            pb::PolicyCandidate {
                reason: Some("Bad".into()),
                ..candidate()
            },
            pb::PolicyCandidate {
                details: vec!["d".into(); MAX_POLICY_LINES + 1],
                ..candidate()
            },
            pb::PolicyCandidate {
                counterexamples: vec!["c".repeat(MAX_POLICY_LINE_BYTES + 1)],
                ..candidate()
            },
            pb::PolicyCandidate {
                widening_examples: vec!["a\nb".into()],
                ..candidate()
            },
            pb::PolicyCandidate {
                revision: Some(3),
                ..candidate()
            },
            pb::PolicyCandidate {
                state: Some(state::QUEUED),
                ..candidate()
            },
            pb::PolicyCandidate {
                queries: Some(MAX_POLICY_QUERIES + 1),
                ..candidate()
            },
        ] {
            assert!(!candidate_valid(&changed));
        }
    }
}
