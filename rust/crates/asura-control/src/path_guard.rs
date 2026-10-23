//! Execution path guard control messages (execution-path-guard.md, control
//! messages; envelope fields 69-72): presence, bounds and reply tokens.
//! Domain errors are `PathGuardReply.error` tokens. Runtime-free.
use crate::{id, pb};
use pb::envelope::Body;

/// Warnings listed in one reply, queue entry or job detail; the rest are
/// counted in the overflow field.
pub const MAX_PATH_WARNINGS: usize = 16;
/// Approved paths per project, and so per reply.
pub const MAX_APPROVED_PATHS: usize = 256;
/// Bytes of one project-relative path.
pub const MAX_GUARD_PATH_BYTES: usize = 1024;
/// The highest path warning kind: 5 `overflow`.
pub const MAX_WARNING_KIND: u32 = 5;

/// Path warning kinds (`PathWarning.kind`).
pub mod kind {
    /// A build or test path a model job changed since the last command.
    pub const MODEL_CHANGED: u32 = 1;
    /// An implicit-execution file with no approval.
    pub const NOT_APPROVED: u32 = 2;
    /// An approved implicit-execution file whose content changed, or that
    /// is missing or cannot be read.
    pub const CHANGED_SINCE_APPROVAL: u32 = 3;
    /// An implicit-execution file over 1 MiB.
    pub const TOO_LARGE: u32 = 4;
    /// A walk, snapshot or listing exceeded its bound.
    pub const OVERFLOW: u32 = 5;
}

/// `PathGuardReply.error` tokens.
pub mod error {
    /// The project is not registered, or the path does not exist (approve)
    /// or is not approved (revoke).
    pub const NOT_FOUND: &str = "not_found";
    /// The path is not a project-relative path: a leading `/`, a `..`
    /// component or a NUL.
    pub const PATH_INVALID: &str = "path_invalid";
    /// The file is over 1 MiB and cannot be approved.
    pub const TOO_LARGE: &str = "too_large";
    /// The path is not in the implicit-execution list.
    pub const NOT_IMPLICIT: &str = "not_implicit";
    /// 256 approved paths, or 4,096 approval and revocation records.
    pub const LIMIT: &str = "limit";
    /// The path guard's request queues are full.
    pub const BUSY: &str = "busy";
    /// The journal, the project directory or the file cannot be read.
    pub const UNAVAILABLE: &str = "unavailable";
    pub const ALL: [&str; 7] = [
        NOT_FOUND,
        PATH_INVALID,
        TOO_LARGE,
        NOT_IMPLICIT,
        LIMIT,
        BUSY,
        UNAVAILABLE,
    ];
}

/// The project-relative path form of kind 32 field 6: 1 to 1,024 bytes,
/// no leading `/`, no NUL and no `..` component. A request path that fails
/// it is answered `path_invalid`; the codec checks only its size so the
/// token can be returned.
pub fn path_form_valid(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= MAX_GUARD_PATH_BYTES
        && !path.starts_with('/')
        && !path.contains('\0')
        && path.split('/').all(|part| part != "..")
}

fn request_path(path: &Option<String>) -> bool {
    path.as_ref()
        .is_some_and(|p| !p.is_empty() && p.len() <= MAX_GUARD_PATH_BYTES)
}

/// One listed warning: a kind 1 to 5 and a valid path.
pub fn warning_valid(w: &pb::PathWarning) -> bool {
    w.kind.is_some_and(|k| (1..=MAX_WARNING_KIND).contains(&k))
        && w.path.as_deref().is_some_and(path_form_valid)
}

/// A warning list with its overflow count: at most 16 valid warnings and a
/// nonzero count of the warnings not listed. A list may be shorter than 16
/// with a count when the service cut it to fit one frame.
pub fn warnings_valid(warnings: &[pb::PathWarning], overflow: Option<u32>) -> bool {
    warnings.len() <= MAX_PATH_WARNINGS
        && warnings.iter().all(warning_valid)
        && overflow.is_none_or(|n| n > 0)
}

/// `PathGuardReply.approved_omitted`: the approved paths the service did
/// not list because the list reached its byte bound. Absent when every path
/// is listed; never zero; the listed and omitted paths together are at most
/// 256, the approvals of one project.
pub fn approved_omitted_valid(listed: usize, omitted: Option<u32>) -> bool {
    omitted.is_none_or(|n| n > 0 && listed.saturating_add(n as usize) <= MAX_APPROVED_PATHS)
}

pub(crate) fn handles(body: &Body) -> bool {
    matches!(
        body,
        Body::PathGuardList(_)
            | Body::PathGuardReply(_)
            | Body::PathGuardApprove(_)
            | Body::PathGuardRevoke(_)
    )
}
pub(crate) fn is_request(body: &Body) -> bool {
    matches!(
        body,
        Body::PathGuardList(_) | Body::PathGuardApprove(_) | Body::PathGuardRevoke(_)
    )
}

pub(crate) fn valid(body: &Body) -> bool {
    match body {
        Body::PathGuardList(v) => id(&v.project_id),
        Body::PathGuardApprove(v) => {
            id(&v.request_id)
                && id(&v.project_id)
                && match v.all {
                    Some(true) => v.path.is_none(),
                    Some(false) | None => request_path(&v.path),
                }
        }
        Body::PathGuardRevoke(v) => id(&v.request_id) && id(&v.project_id) && request_path(&v.path),
        Body::PathGuardReply(v) => match v.error.as_deref() {
            Some(token) => {
                error::ALL.contains(&token)
                    && v.pending.is_empty()
                    && v.overflow.is_none()
                    && v.approved.is_empty()
                    && v.approved_omitted.is_none()
            }
            None => {
                warnings_valid(&v.pending, v.overflow)
                    && v.approved.len() <= MAX_APPROVED_PATHS
                    && approved_omitted_valid(v.approved.len(), v.approved_omitted)
                    && v.approved.iter().all(|p| path_form_valid(p))
                    && v.approved
                        .windows(2)
                        .all(|w| w[0].as_bytes() < w[1].as_bytes())
            }
        },
        _ => false,
    }
}

/// EPG-U6: the control codecs of fields 69-72 and queue entry fields
/// 21-22, at their bounds, and each error token.
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
    /// Encoded, decoded by the wire scan and equal, and valid in its
    /// direction.
    fn round_trips(body: Body, direction: Direction) -> bool {
        let value = envelope(body);
        let Ok(frame) = encode_frame(&value) else {
            return false;
        };
        decode_body(&frame[HEADER_BYTES..]).is_ok_and(|decoded| decoded == value)
            && validate_semantics(&value, direction).is_ok()
    }
    fn warning(kind: u32, path: &str) -> pb::PathWarning {
        pb::PathWarning {
            kind: Some(kind),
            path: Some(path.into()),
        }
    }
    fn approve(path: Option<&str>, all: Option<bool>) -> Body {
        Body::PathGuardApprove(pb::PathGuardApprove {
            request_id: Some(vec![3; 16]),
            project_id: Some(vec![4; 16]),
            path: path.map(str::to_owned),
            all,
        })
    }
    fn revoke(path: Option<&str>) -> Body {
        Body::PathGuardRevoke(pb::PathGuardRevoke {
            request_id: Some(vec![3; 16]),
            project_id: Some(vec![4; 16]),
            path: path.map(str::to_owned),
        })
    }
    fn listed(pending: Vec<pb::PathWarning>, overflow: Option<u32>, approved: Vec<String>) -> Body {
        Body::PathGuardReply(pb::PathGuardReply {
            pending,
            overflow,
            approved,
            error: None,
            approved_omitted: None,
        })
    }

    #[test]
    fn epg_u6_requests_and_direction() {
        let list = Body::PathGuardList(pb::PathGuardList {
            project_id: Some(vec![4; 16]),
        });
        assert!(round_trips(list.clone(), Direction::ClientToServer));
        assert!(!reply(list));
        assert!(!request(Body::PathGuardList(pb::PathGuardList {
            project_id: None
        })));
        assert!(!request(Body::PathGuardList(pb::PathGuardList {
            project_id: Some(vec![0; 16])
        })));
        // Approve: a path or `all`, never both, never neither.
        let long = "p".repeat(MAX_GUARD_PATH_BYTES);
        for accepted in [
            approve(Some(".envrc"), None),
            approve(Some(".envrc"), Some(false)),
            approve(Some(&long), None),
            approve(None, Some(true)),
            // The form is the service's `path_invalid`, not a fault.
            approve(Some("/abs"), None),
            approve(Some("a/../b"), None),
        ] {
            assert!(round_trips(accepted, Direction::ClientToServer));
        }
        let over = "p".repeat(MAX_GUARD_PATH_BYTES + 1);
        for rejected in [
            approve(None, None),
            approve(None, Some(false)),
            approve(Some(".envrc"), Some(true)),
            approve(Some(""), None),
            approve(Some(&over), None),
        ] {
            assert!(!request(rejected));
        }
        assert!(!request(Body::PathGuardApprove(pb::PathGuardApprove {
            request_id: None,
            project_id: Some(vec![4; 16]),
            path: Some(".envrc".into()),
            all: None,
        })));
        assert!(!reply(approve(Some(".envrc"), None)));
        // Revoke: a path, always.
        assert!(round_trips(
            revoke(Some(".git/config")),
            Direction::ClientToServer
        ));
        assert!(round_trips(revoke(Some(&long)), Direction::ClientToServer));
        assert!(!request(revoke(None)));
        assert!(!request(revoke(Some(""))));
        assert!(!request(revoke(Some(&over))));
        assert!(!request(Body::PathGuardRevoke(pb::PathGuardRevoke {
            request_id: Some(vec![3; 16]),
            project_id: None,
            path: Some(".envrc".into()),
        })));
    }

    #[test]
    fn epg_u6_path_form() {
        assert!(path_form_valid(".git/hooks/pre-commit"));
        assert!(path_form_valid("."));
        assert!(path_form_valid(&"p".repeat(MAX_GUARD_PATH_BYTES)));
        for bad in ["", "/abs", "a/../b", "..", "a\0b"] {
            assert!(!path_form_valid(bad), "{bad:?}");
        }
        assert!(!path_form_valid(&"p".repeat(MAX_GUARD_PATH_BYTES + 1)));
    }

    #[test]
    fn epg_u6_reply_bounds_and_errors() {
        let sixteen: Vec<_> = (0..MAX_PATH_WARNINGS)
            .map(|i| warning(1 + (i as u32 % MAX_WARNING_KIND), &format!("p{i}")))
            .collect();
        let approved: Vec<String> = (0..MAX_APPROVED_PATHS)
            .map(|i| format!("a/{i:04}/.envrc"))
            .collect();
        assert!(round_trips(
            listed(sixteen.clone(), Some(7), approved.clone()),
            Direction::ServerToClient
        ));
        assert!(round_trips(
            listed(Vec::new(), None, Vec::new()),
            Direction::ServerToClient
        ));
        assert!(!request(listed(Vec::new(), None, Vec::new())));
        // A cut list keeps its count; a zero count is not sent.
        assert!(reply(listed(sixteen[..3].to_vec(), Some(13), Vec::new())));
        assert!(!reply(listed(sixteen.clone(), Some(0), Vec::new())));
        // 17 warnings or 257 approvals are malformed on the wire.
        let mut seventeen = sixteen.clone();
        seventeen.push(warning(1, "x"));
        assert!(!reply(listed(seventeen.clone(), None, Vec::new())));
        assert!(encode_frame(&envelope(listed(seventeen, None, Vec::new()))).is_err());
        let mut many: Vec<String> = (0..=MAX_APPROVED_PATHS)
            .map(|i| format!("{i:04}"))
            .collect();
        assert!(!reply(listed(Vec::new(), None, many.clone())));
        assert!(encode_frame(&envelope(listed(Vec::new(), None, many.clone()))).is_err());
        // Approved paths are valid, unique and in byte order.
        many.truncate(2);
        many.reverse();
        assert!(!reply(listed(Vec::new(), None, many)));
        assert!(!reply(listed(
            Vec::new(),
            None,
            vec!["a".into(), "a".into()]
        )));
        assert!(!reply(listed(Vec::new(), None, vec!["/a".into()])));
        // Each kind 1-5; 0 and 6 are not warnings; each path valid.
        for kind in 1..=MAX_WARNING_KIND {
            assert!(reply(listed(
                vec![warning(kind, "build.rs")],
                None,
                Vec::new()
            )));
        }
        for bad in [
            warning(0, "build.rs"),
            warning(MAX_WARNING_KIND + 1, "build.rs"),
            warning(1, "/build.rs"),
            warning(1, "../build.rs"),
            warning(1, ""),
            pb::PathWarning {
                kind: None,
                path: Some("build.rs".into()),
            },
            pb::PathWarning {
                kind: Some(1),
                path: None,
            },
        ] {
            assert!(
                !reply(listed(vec![bad.clone()], None, Vec::new())),
                "{bad:?}"
            );
        }
        // Each error token alone; nothing else beside it.
        for token in error::ALL {
            let alone = Body::PathGuardReply(pb::PathGuardReply {
                error: Some(token.into()),
                ..Default::default()
            });
            assert!(round_trips(alone, Direction::ServerToClient), "{token}");
            for crowded in [
                pb::PathGuardReply {
                    error: Some(token.into()),
                    pending: vec![warning(1, "x")],
                    ..Default::default()
                },
                pb::PathGuardReply {
                    error: Some(token.into()),
                    overflow: Some(1),
                    ..Default::default()
                },
                pb::PathGuardReply {
                    error: Some(token.into()),
                    approved: vec![".envrc".into()],
                    ..Default::default()
                },
            ] {
                assert!(!reply(Body::PathGuardReply(crowded)), "{token}");
            }
        }
        assert!(!reply(Body::PathGuardReply(pb::PathGuardReply {
            error: Some("denied".into()),
            ..Default::default()
        })));
    }

    /// `approved_omitted` (field 5): a nonzero count beside a cut list,
    /// listed and omitted together at most 256, never beside an error.
    #[test]
    fn epg_u6_approved_omitted() {
        let with = |approved: Vec<String>, omitted: Option<u32>| {
            Body::PathGuardReply(pb::PathGuardReply {
                approved,
                approved_omitted: omitted,
                ..Default::default()
            })
        };
        let forty: Vec<String> = (0..40).map(|i| format!("a/{i:04}/.envrc")).collect();
        assert!(round_trips(
            with(forty.clone(), Some(216)),
            Direction::ServerToClient
        ));
        assert!(round_trips(
            with(forty.clone(), None),
            Direction::ServerToClient
        ));
        assert!(reply(with(Vec::new(), Some(256))));
        assert!(!reply(with(forty.clone(), Some(0))), "zero is not sent");
        assert!(
            !reply(with(forty.clone(), Some(217))),
            "more than 256 in all"
        );
        assert!(!reply(with(Vec::new(), Some(257))));
        assert!(!reply(Body::PathGuardReply(pb::PathGuardReply {
            error: Some(error::LIMIT.into()),
            approved_omitted: Some(1),
            ..Default::default()
        })));
        assert!(approved_omitted_valid(256, None));
        assert!(!approved_omitted_valid(256, Some(1)));
        assert!(approved_omitted_valid(255, Some(1)));
    }

    /// The largest reply that the service sends fits one frame: 16
    /// warnings of 1,024 bytes; approvals are cut to the frame by the
    /// service.
    #[test]
    fn epg_u6_largest_warning_list_fits_a_frame() {
        let path = "p".repeat(MAX_GUARD_PATH_BYTES);
        let pending = vec![warning(MAX_WARNING_KIND, &path); MAX_PATH_WARNINGS];
        let value = envelope(listed(pending, Some(u32::MAX), Vec::new()));
        assert!(prost::Message::encoded_len(&value) <= MAX_FRAME_BYTES);
        assert!(round_trips(value.body.unwrap(), Direction::ServerToClient));
    }

    fn command_entry() -> pb::ConversationQueueEntry {
        pb::ConversationQueueEntry {
            input_id: Some(vec![3; 16]),
            project_id: Some(vec![4; 16]),
            conversation_id: Some(vec![5; 16]),
            target_generation: Some(2),
            kind: Some(crate::person::ENTRY_KIND),
            state: Some(1),
            text: Some("make test".into()),
            sequence: Some(1),
            new_conversation: Some(false),
            order_position: Some(1),
            command_cwd: Some(".".into()),
            command_outside_sandbox: Some(true),
            ..Default::default()
        }
    }
    fn entries(entry: pb::ConversationQueueEntry) -> Body {
        Body::ConversationQueueReply(pb::ConversationQueueReply {
            entries: vec![entry],
            full_text: Some(false),
            revision: Some(1),
            pending: Some(false),
            order_revision: Some(1),
            stale_order: Some(false),
            ..Default::default()
        })
    }

    #[test]
    fn epg_u6_queue_entry_fields_21_22_and_wait_reason_15() {
        let sixteen: Vec<_> = (0..MAX_PATH_WARNINGS)
            .map(|i| warning(1, &format!("tests/t{i}.rs")))
            .collect();
        let entry = pb::ConversationQueueEntry {
            path_warnings: sixteen.clone(),
            path_overflow: Some(3),
            ..command_entry()
        };
        assert!(round_trips(
            entries(entry.clone()),
            Direction::ServerToClient
        ));
        // Held with `paths_changed` and its warnings.
        assert!(reply(entries(pb::ConversationQueueEntry {
            state: Some(2),
            hold_reason: Some("paths_changed".into()),
            ..entry.clone()
        })));
        // Waiting with `path_check`.
        assert!(round_trips(
            entries(pb::ConversationQueueEntry {
                waiting_reason: Some(pb::QueueWaitReason::PathCheck as i32),
                waiting_text: Some("waiting: path check".into()),
                ..command_entry()
            }),
            Direction::ServerToClient
        ));
        assert_eq!(pb::QueueWaitReason::PathCheck as i32, 15);
        // Kind 3 only.
        let message = pb::ConversationQueueEntry {
            kind: Some(1),
            command_cwd: None,
            command_outside_sandbox: None,
            ..command_entry()
        };
        assert!(reply(entries(message.clone())));
        assert!(!reply(entries(pb::ConversationQueueEntry {
            path_warnings: vec![warning(1, "build.rs")],
            ..message.clone()
        })));
        assert!(!reply(entries(pb::ConversationQueueEntry {
            path_overflow: Some(1),
            ..message
        })));
        // Bounds and forms.
        let mut seventeen = sixteen.clone();
        seventeen.push(warning(1, "x"));
        assert!(!reply(entries(pb::ConversationQueueEntry {
            path_warnings: seventeen,
            ..command_entry()
        })));
        assert!(!reply(entries(pb::ConversationQueueEntry {
            path_overflow: Some(0),
            ..entry.clone()
        })));
        assert!(!reply(entries(pb::ConversationQueueEntry {
            path_warnings: vec![warning(6, "build.rs")],
            ..command_entry()
        })));
    }

    #[test]
    fn epg_u6_job_detail_path_warnings() {
        let detail = |warnings: Vec<pb::PathWarning>, error: Option<&str>| {
            Body::JobDetail(pb::JobDetail {
                entry: None,
                command: None,
                cwd: None,
                notes: Vec::new(),
                error: error.map(str::to_owned),
                path_warnings: warnings,
            })
        };
        assert!(reply(detail(Vec::new(), Some("not_found"))));
        assert!(!reply(detail(
            vec![warning(1, "build.rs")],
            Some("not_found")
        )));
        let sixteen = vec![warning(2, ".envrc"); MAX_PATH_WARNINGS];
        assert!(
            encode_frame(&envelope(detail(
                [sixteen.clone(), vec![warning(1, "x")]].concat(),
                None,
            )))
            .is_err()
        );
        let frame = encode_frame(&envelope(detail(sixteen, None))).unwrap();
        assert!(decode_body(&frame[HEADER_BYTES..]).is_ok());
    }
}
