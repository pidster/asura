//! Named secret control messages (named-secrets.md, control API; envelope
//! fields 73-76): presence and byte bounds. The service checks the secret
//! name rule and answers `INVALID`, so the control crate has no copy of it.
//! Only `SecretPut` carries a value; [`scrub`] clears it. Runtime-free.
use crate::pb;
use pb::envelope::Body;
pub use zeroize::{Zeroize, Zeroizing};

/// Bytes of a secret name.
pub const MAX_NAME_BYTES: usize = 48;
/// Bytes of a named secret value.
pub const MAX_VALUE_BYTES: usize = 4_096;
/// Names in one list reply.
pub const MAX_LISTED_NAMES: usize = 64;

fn name(value: &Option<String>) -> bool {
    value
        .as_ref()
        .is_some_and(|n| !n.is_empty() && n.len() <= MAX_NAME_BYTES)
}

pub(crate) fn handles(body: &Body) -> bool {
    matches!(
        body,
        Body::SecretPut(_) | Body::SecretList(_) | Body::SecretRemove(_) | Body::SecretReply(_)
    )
}
pub(crate) fn is_request(body: &Body) -> bool {
    matches!(
        body,
        Body::SecretPut(_) | Body::SecretList(_) | Body::SecretRemove(_)
    )
}

pub(crate) fn valid(body: &Body) -> bool {
    use pb::SecretResult as R;
    match body {
        Body::SecretPut(v) => {
            name(&v.name)
                && v.value
                    .as_ref()
                    .is_some_and(|b| !b.is_empty() && b.len() <= MAX_VALUE_BYTES)
                && v.replace.is_some()
        }
        Body::SecretList(_) => true,
        Body::SecretRemove(v) => name(&v.name),
        Body::SecretReply(v) => {
            let Some(result) = v.result.and_then(|r| R::try_from(r).ok()) else {
                return false;
            };
            result != R::Unspecified
                && (result == R::Listed || v.names.is_empty())
                && v.names.len() <= MAX_LISTED_NAMES
                && v.names
                    .iter()
                    .all(|n| !n.is_empty() && n.len() <= MAX_NAME_BYTES)
                && match result {
                    R::Locked | R::Unavailable | R::Denied | R::ReplaceFailed => {
                        v.status.is_some_and(|s| s > 0)
                    }
                    _ => v.status.is_none(),
                }
        }
        _ => false,
    }
}

/// Clears a `SecretPut` value in `envelope` before it is dropped.
pub fn scrub(envelope: &mut pb::Envelope) {
    if let Some(Body::SecretPut(v)) = envelope.body.as_mut()
        && let Some(value) = v.value.as_mut()
    {
        value.zeroize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Direction, encode_frame, validate_semantics};

    fn envelope(body: Body) -> pb::Envelope {
        pb::Envelope {
            service_epoch: Some(vec![1; 16]),
            attachment_id: Some(vec![2; 16]),
            request_counter: Some(1),
            body: Some(body),
        }
    }
    fn put(name: &str, value: usize) -> Body {
        Body::SecretPut(pb::SecretPut {
            name: Some(name.into()),
            value: Some(vec![7; value]),
            replace: Some(false),
        })
    }

    /// NS-U8: shape, bounds, direction and `scrub`.
    #[test]
    fn ns_u8_secret_messages() {
        let ok = |b: Body, d| validate_semantics(&envelope(b), d).is_ok();
        assert!(ok(put("GITHUB_TOKEN", 1), Direction::ClientToServer));
        assert!(ok(put("A", MAX_VALUE_BYTES), Direction::ClientToServer));
        assert!(!ok(put("A", 0), Direction::ClientToServer));
        assert!(!ok(
            put("A", MAX_VALUE_BYTES + 1),
            Direction::ClientToServer
        ));
        assert!(!ok(put(&"A".repeat(49), 1), Direction::ClientToServer));
        assert!(!ok(put("A", 1), Direction::ServerToClient));
        assert!(ok(
            Body::SecretList(pb::SecretList {}),
            Direction::ClientToServer
        ));
        assert!(ok(
            Body::SecretRemove(pb::SecretRemove {
                name: Some("A".into())
            }),
            Direction::ClientToServer
        ));
        assert!(!ok(
            Body::SecretRemove(pb::SecretRemove { name: None }),
            Direction::ClientToServer
        ));
        let reply = |result: pb::SecretResult, names: Vec<String>, status| {
            Body::SecretReply(pb::SecretReply {
                result: Some(result as i32),
                names,
                status,
            })
        };
        let s = Direction::ServerToClient;
        assert!(ok(
            reply(pb::SecretResult::Listed, vec!["A".into()], None),
            s
        ));
        assert!(!ok(
            reply(pb::SecretResult::Stored, vec!["A".into()], None),
            s
        ));
        assert!(ok(reply(pb::SecretResult::Locked, vec![], Some(25_308)), s));
        assert!(!ok(reply(pb::SecretResult::Locked, vec![], None), s));
        assert!(!ok(reply(pb::SecretResult::Stored, vec![], Some(1)), s));
        assert!(!ok(reply(pb::SecretResult::Unspecified, vec![], None), s));
        let many = (0..65).map(|i| format!("N{i}")).collect();
        assert!(!ok(reply(pb::SecretResult::Listed, many, None), s));
        // A frame round-trips; `scrub` leaves no value byte.
        let mut message = envelope(put("A", 32));
        assert!(encode_frame(&message).is_ok());
        scrub(&mut message);
        let Some(Body::SecretPut(v)) = &message.body else {
            unreachable!()
        };
        assert!(v.value.as_ref().is_some_and(|b| b.iter().all(|x| *x == 0)));
    }
}
