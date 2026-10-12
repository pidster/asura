//! Scheduling at controlled writer boundaries; no disk or model completion claims.
use super::*;
use std::time::Duration;

fn envelope(body: Body, counter: u64) -> pb::Envelope {
    pb::Envelope {
        service_epoch: Some(vec![1; 16]),
        attachment_id: Some(vec![2; 16]),
        request_counter: Some(counter),
        body: Some(body),
    }
}
fn history() -> Body {
    Body::ConversationHistory(pb::ConversationHistory {
        project_id: Some(vec![3; 16]),
        limit: Some(8),
        ..Default::default()
    })
}
fn prompt() -> Body {
    Body::ConversationReadPrompt(pb::ConversationReadPrompt {
        project_id: Some(vec![3; 16]),
        operation_id: Some(vec![4; 16]),
    })
}
fn queue() -> Body {
    Body::ConversationQueueList(pb::ConversationQueueList {
        project_id: Some(vec![3; 16]),
        ..Default::default()
    })
}
fn error_message(response: pb::Envelope) -> String {
    let Some(Body::Error(value)) = response.body else {
        panic!("expected explicit error")
    };
    value.message.unwrap()
}

#[test]
fn foreground_reads_wait_for_every_pending_read_kind_in_fifo_order() {
    for pending_kind in 0..4 {
        let mut fixture = tool_tests::Fixture::new();
        let owner = &mut fixture.owner;
        owner.active = None;
        let now = Instant::now();
        let request = envelope(history(), 100);
        let job = match pending_kind {
            0 => Job::QueueList(request.clone()),
            1 => Job::History(request.clone()),
            2 => Job::ReadPrompt(request.clone()),
            _ => Job::Read(request),
        };
        let (ticket, controller) =
            writer::test_support::controlled_ticket(now + Duration::from_secs(2), false);
        owner.pending = Some((ticket, job));
        for (counter, body) in [(1, history()), (2, prompt()), (3, queue())] {
            assert!(owner.request(envelope(body, counter), now).is_none());
        }
        assert_eq!(owner.foreground.len(), 3);
        owner.dispatch_foreground(now);
        assert_eq!(owner.foreground.len(), 3);
        assert!(owner.out.is_empty());
        owner.pending = None;
        controller.finish();
        // The fixture has no writer. Dispatch proves order with explicit
        // unavailable replies, without fabricating successful journal reads.
        for counter in 1..=3 {
            owner.dispatch_foreground(now);
            let response = owner.out.pop_front().unwrap();
            assert_eq!(response.request_counter, Some(counter));
            assert_eq!(error_message(response), "storage_unavailable");
        }
        assert!(owner.foreground.is_empty());
    }
}

#[test]
fn foreground_fifo_has_exact_bound_and_expires_without_dispatch() {
    let mut fixture = tool_tests::Fixture::new();
    let owner = &mut fixture.owner;
    let now = Instant::now();
    let (ticket, controller) =
        writer::test_support::controlled_ticket(now + Duration::from_secs(3), false);
    owner.pending = Some((ticket, Job::Read(envelope(history(), 100))));
    for counter in 1..=32 {
        assert!(owner.request(envelope(history(), counter), now).is_none());
    }
    assert_eq!(
        error_message(owner.request(envelope(history(), 33), now).unwrap()),
        "conversation_busy"
    );
    owner.dispatch_foreground(now + Duration::from_millis(1999));
    assert_eq!(owner.foreground.len(), 32);
    assert!(owner.out.is_empty());
    owner.dispatch_foreground(now + Duration::from_secs(2));
    assert!(owner.foreground.is_empty());
    for counter in 1..=32 {
        let response = owner.out.pop_front().unwrap();
        assert_eq!(response.request_counter, Some(counter));
        assert_eq!(error_message(response), "storage_timeout");
    }
    owner.pending = None;
    controller.finish();
}

#[test]
fn foreground_drain_is_explicit_and_does_not_wait_for_writer() {
    let mut fixture = tool_tests::Fixture::new();
    let owner = &mut fixture.owner;
    let now = Instant::now();
    let (ticket, controller) =
        writer::test_support::controlled_ticket(now + Duration::from_secs(2), false);
    owner.pending = Some((ticket, Job::Read(envelope(history(), 100))));
    assert!(owner.request(envelope(prompt(), 1), now).is_none());
    owner.closing = true;
    owner.dispatch_foreground(now);
    assert!(owner.foreground.is_empty());
    assert_eq!(
        error_message(owner.out.pop_front().unwrap()),
        "service_draining"
    );
    owner.pending = None;
    controller.finish();
}

fn active_projection(owner: &Owner, now: Instant) -> Arc<journal::Replay> {
    let bytes = journal::encode_frame(
        &journal::FrameContext {
            installation_id: [1; 16],
            transition_id: [2; 16],
            sequence: 1,
            prior_digest: [0; 32],
            expected_revision: 0,
            owner_generation: 1,
        },
        &journal::Record::PendingInit(journal::PendingInit {
            request: [3; 16],
            digest: journal::request_digest(journal::Request::Initialize { mode: 1 }).unwrap(),
            mode: 1,
            configuration_revision: 1,
            configuration_digest: [4; 32],
            graph: [5; 16],
        }),
    )
    .unwrap();
    let mut replay = journal::replay(
        &bytes,
        now + Duration::from_secs(2),
        &std::sync::atomic::AtomicBool::new(false),
    )
    .unwrap();
    let active = owner.active.as_ref().unwrap();
    replay.operations.insert(
        active.turn.operation,
        journal::Operation {
            reserved_output_tokens: active.turn.reserved_output_tokens,
            conversation: active.turn.conversation,
            generation: active.turn.generation,
            task: active.turn.task,
            owner_generation: 1,
            input_digest: active.turn.input_digest,
            accepted_frame: 0..1,
            helper: None,
            cancel: None,
            terminal: None,
            tools: vec![],
        },
    );
    Arc::new(replay)
}

#[test]
fn active_observation_and_cancel_bypass_pending_foreground_reads() {
    let mut fixture = tool_tests::Fixture::new();
    let owner = &mut fixture.owner;
    let now = Instant::now();
    owner.replay = Some(active_projection(owner, now));
    let (ticket, controller) =
        writer::test_support::controlled_ticket(now + Duration::from_secs(2), false);
    owner.pending = Some((ticket, Job::Read(envelope(history(), 100))));
    assert!(owner.request(envelope(history(), 1), now).is_none());
    let response = owner
        .request(
            envelope(
                Body::ConversationObserve(pb::ConversationObserve {
                    operation_id: Some(vec![4; 16]),
                    after_cursor: Some(0),
                    wait_ms: Some(0),
                }),
                2,
            ),
            now,
        )
        .unwrap();
    assert!(matches!(response.body, Some(Body::ConversationEvent(_))));
    // Unknown-operation cancellation still resolves immediately. Its identity
    // check must not enter the read FIFO or wait for the writer.
    let response = owner
        .request(
            envelope(
                Body::ConversationCancel(pb::ConversationCancel {
                    operation_id: Some(vec![9; 16]),
                    generation: Some(1),
                }),
                3,
            ),
            now,
        )
        .unwrap();
    assert_eq!(error_message(response), "unknown_operation");
    assert!(
        owner
            .request(
                envelope(
                    Body::ConversationCancel(pb::ConversationCancel {
                        operation_id: Some(vec![4; 16]),
                        generation: Some(1),
                    }),
                    4,
                ),
                now,
            )
            .is_none()
    );
    let active = owner.active.as_ref().unwrap();
    assert_eq!(active.cancel, Some(journal::Cause::UserCancel));
    assert_eq!(active.cancel_replies.len(), 1);
    assert_eq!(active.cancel_replies[0].request_counter, Some(4));
    assert_eq!(owner.foreground.len(), 1);
    owner.pending = None;
    controller.finish();
}
