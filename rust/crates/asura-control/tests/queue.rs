use asura_control::{Direction, HEADER_BYTES, decode_body, encode_frame, pb, validate_semantics};
use pb::envelope::Body;
fn envelope(body: Body) -> pb::Envelope {
    pb::Envelope {
        service_epoch: Some(vec![1; 16]),
        attachment_id: Some(vec![2; 16]),
        request_counter: Some(1),
        body: Some(body),
    }
}
fn request() -> pb::ConversationEnqueue {
    pb::ConversationEnqueue {
        request_id: Some(vec![3; 16]),
        project_id: Some(vec![4; 16]),
        conversation_id: Some(vec![5; 16]),
        target_operation_id: Some(vec![6; 16]),
        target_generation: Some(1),
        kind: Some(1),
        prompt: Some("follow-up".into()),
    }
}
#[test]
fn queue_roundtrips_with_strict_identity_direction_and_bounds() {
    let value = envelope(Body::ConversationEnqueue(request()));
    assert!(validate_semantics(&value, Direction::ClientToServer).is_ok());
    assert!(validate_semantics(&value, Direction::ServerToClient).is_err());
    let frame = encode_frame(&value).unwrap();
    assert_eq!(decode_body(&frame[HEADER_BYTES..]).unwrap(), value);
    for changed in [
        pb::ConversationEnqueue {
            kind: Some(3),
            ..request()
        },
        pb::ConversationEnqueue {
            target_generation: Some(0),
            ..request()
        },
        pb::ConversationEnqueue {
            target_operation_id: None,
            ..request()
        },
        pb::ConversationEnqueue {
            prompt: Some("x".repeat(32769)),
            ..request()
        },
    ] {
        assert!(
            validate_semantics(
                &envelope(Body::ConversationEnqueue(changed)),
                Direction::ClientToServer
            )
            .is_err()
        );
    }
}
#[test]
fn queue_projection_requires_bounded_text_and_truthful_operation_state() {
    let entry = pb::ConversationQueueEntry {
        input_id: Some(vec![1; 16]),
        project_id: Some(vec![2; 16]),
        conversation_id: Some(vec![3; 16]),
        target_operation_id: Some(vec![4; 16]),
        target_generation: Some(1),
        kind: Some(1),
        state: Some(1),
        text: Some("queued".into()),
        operation_id: None,
        generation: None,
        sequence: Some(5),
        new_conversation: None,
        order_position: None,
        hold_reason: None,
    };
    let valid = |entries, full| {
        validate_semantics(
            &envelope(Body::ConversationQueueReply(pb::ConversationQueueReply {
                entries,
                request_id: None,
                full_text: Some(full),
                revision: None,
                pending: None,
                order_revision: None,
                stale_order: None,
                accepted_input_id: None,
            })),
            Direction::ServerToClient,
        )
        .is_ok()
    };
    assert!(valid(vec![entry.clone()], false));
    assert!(!valid(vec![entry.clone(); 17], false));
    assert!(!valid(vec![entry.clone(); 2], true));
    assert!(!valid(
        vec![pb::ConversationQueueEntry {
            state: Some(3),
            ..entry.clone()
        }],
        false
    ));
    assert!(!valid(
        vec![pb::ConversationQueueEntry {
            text: Some("x".repeat(257)),
            ..entry.clone()
        }],
        false
    ));
    assert!(valid(
        vec![pb::ConversationQueueEntry {
            text: Some("x".repeat(32768)),
            ..entry
        }],
        true
    ));
}

#[test]
fn new_queue_requests_roundtrip_and_reject_malformed_cursor_or_move() {
    let submit = pb::ConversationQueueSubmit {
        request_id: Some(vec![3; 16]),
        project_id: Some(vec![4; 16]),
        conversation_id: None,
        expected_generation: Some(0),
        new_conversation: Some(true),
        prompt: Some("first".into()),
    };
    let body = Body::ConversationQueueSubmit(submit.clone());
    let value = envelope(body.clone());
    assert!(validate_semantics(&value, Direction::ClientToServer).is_ok());
    assert!(validate_semantics(&value, Direction::ServerToClient).is_err());
    assert_eq!(
        decode_body(&encode_frame(&value).unwrap()[HEADER_BYTES..]).unwrap(),
        value
    );
    for changed in [
        pb::ConversationQueueSubmit {
            new_conversation: None,
            ..submit.clone()
        },
        pb::ConversationQueueSubmit {
            new_conversation: Some(false),
            ..submit.clone()
        },
        pb::ConversationQueueSubmit {
            expected_generation: Some(1),
            ..submit.clone()
        },
        pb::ConversationQueueSubmit {
            conversation_id: Some(vec![8; 16]),
            ..submit.clone()
        },
        pb::ConversationQueueSubmit {
            prompt: Some(String::new()),
            ..submit.clone()
        },
        pb::ConversationQueueSubmit {
            prompt: Some("x".repeat(32769)),
            ..submit.clone()
        },
    ] {
        assert!(
            validate_semantics(
                &envelope(Body::ConversationQueueSubmit(changed)),
                Direction::ClientToServer
            )
            .is_err()
        );
    }
    let existing = pb::ConversationQueueSubmit {
        conversation_id: Some(vec![8; 16]),
        expected_generation: Some(0),
        new_conversation: Some(false),
        ..submit
    };
    assert!(
        validate_semantics(
            &envelope(Body::ConversationQueueSubmit(existing)),
            Direction::ClientToServer
        )
        .is_ok()
    );

    let reorder = pb::ConversationQueueReorder {
        request_id: Some(vec![3; 16]),
        input_id: Some(vec![4; 16]),
        after_input_id: Some(vec![5; 16]),
        expected_order_revision: Some(0),
    };
    let value = envelope(Body::ConversationQueueReorder(reorder.clone()));
    assert!(validate_semantics(&value, Direction::ClientToServer).is_ok());
    assert_eq!(
        decode_body(&encode_frame(&value).unwrap()[HEADER_BYTES..]).unwrap(),
        value
    );
    for changed in [
        pb::ConversationQueueReorder {
            after_input_id: reorder.input_id.clone(),
            ..reorder.clone()
        },
        pb::ConversationQueueReorder {
            expected_order_revision: None,
            ..reorder.clone()
        },
        pb::ConversationQueueReorder {
            after_input_id: Some(vec![0; 16]),
            ..reorder.clone()
        },
    ] {
        assert!(
            validate_semantics(
                &envelope(Body::ConversationQueueReorder(changed)),
                Direction::ClientToServer
            )
            .is_err()
        );
    }
    assert!(
        validate_semantics(
            &envelope(Body::ConversationQueueReorder(
                pb::ConversationQueueReorder {
                    after_input_id: None,
                    ..reorder
                }
            )),
            Direction::ClientToServer
        )
        .is_ok()
    );
}

#[test]
fn queue_v2_projection_and_stale_order_are_strict() {
    let entry = pb::ConversationQueueEntry {
        input_id: Some(vec![1; 16]),
        project_id: Some(vec![2; 16]),
        conversation_id: Some(vec![3; 16]),
        target_operation_id: None,
        target_generation: Some(0),
        kind: Some(1),
        state: Some(1),
        text: Some("first".into()),
        operation_id: None,
        generation: None,
        sequence: Some(1),
        new_conversation: Some(true),
        order_position: Some(1),
        hold_reason: None,
    };
    let reply = pb::ConversationQueueReply {
        entries: vec![entry.clone()],
        request_id: Some(vec![4; 16]),
        full_text: Some(false),
        revision: Some(1),
        pending: Some(false),
        order_revision: Some(1),
        stale_order: Some(true),
        accepted_input_id: None,
    };
    assert!(
        validate_semantics(
            &envelope(Body::ConversationQueueReply(reply.clone())),
            Direction::ServerToClient
        )
        .is_ok()
    );
    for changed in [
        pb::ConversationQueueReply {
            order_revision: None,
            ..reply.clone()
        },
        pb::ConversationQueueReply {
            request_id: None,
            ..reply.clone()
        },
        pb::ConversationQueueReply {
            full_text: Some(true),
            ..reply.clone()
        },
        pb::ConversationQueueReply {
            entries: vec![pb::ConversationQueueEntry {
                target_generation: Some(1),
                ..entry.clone()
            }],
            ..reply.clone()
        },
        pb::ConversationQueueReply {
            entries: vec![pb::ConversationQueueEntry {
                new_conversation: None,
                ..entry
            }],
            ..reply.clone()
        },
    ] {
        assert!(
            validate_semantics(
                &envelope(Body::ConversationQueueReply(changed)),
                Direction::ServerToClient
            )
            .is_err()
        );
    }
}

#[test]
fn promotion_requires_exact_target_and_ordinary_decisions_forbid_it() {
    let mut decision = pb::ConversationQueueDecision {
        request_id: Some(vec![3; 16]),
        input_id: Some(vec![4; 16]),
        action: Some(3),
        target_operation_id: Some(vec![5; 16]),
        target_generation: Some(1),
    };
    let valid = |v: pb::ConversationQueueDecision| {
        validate_semantics(
            &envelope(Body::ConversationQueueDecision(v)),
            Direction::ClientToServer,
        )
        .is_ok()
    };
    assert!(valid(decision.clone()));
    let frame = encode_frame(&envelope(Body::ConversationQueueDecision(decision.clone()))).unwrap();
    assert_eq!(
        decode_body(&frame[HEADER_BYTES..]).unwrap().body,
        Some(Body::ConversationQueueDecision(decision.clone()))
    );
    decision.target_generation = Some(0);
    assert!(!valid(decision.clone()));
    decision.target_generation = Some(1);
    decision.target_operation_id = None;
    assert!(!valid(decision.clone()));
    decision.action = Some(1);
    assert!(!valid(decision.clone()));
    decision.target_generation = None;
    assert!(valid(decision.clone()));
    decision.action = Some(2);
    assert!(valid(decision.clone()));
    decision.action = Some(4);
    assert!(!valid(decision));
}
