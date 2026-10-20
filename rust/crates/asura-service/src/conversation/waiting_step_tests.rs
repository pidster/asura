//! WS-01..05: the waiting step is live display state derived from owner state.
//! No helper or writer is spawned; the process journey covers real commits.
use super::*;
use crate::model_owner::HelperStep;
use pb::ConversationWaitStep as Step;
use std::time::Duration;

const INPUT: journal::Id = [9; 16];
const OPERATION: journal::Id = [4; 16];

fn owner_at(step: Option<HelperStep>, accepted: bool) -> tool_tests::Fixture {
    let mut fixture = tool_tests::Fixture::new();
    let active = fixture.owner.active.as_mut().unwrap();
    active.model = ModelOwner::step_test_fixture(step);
    active.accepted = accepted;
    active.tool = None;
    active.queue_input = Some(INPUT);
    fixture
}
fn envelope(body: Body) -> pb::Envelope {
    pb::Envelope {
        service_epoch: Some(vec![1; 16]),
        attachment_id: Some(vec![2; 16]),
        request_counter: Some(7),
        body: Some(body),
    }
}
fn event(reply: pb::Envelope) -> pb::ConversationEvent {
    asura_control::validate_semantics(&reply, asura_control::Direction::ServerToClient)
        .expect("valid event");
    let Some(Body::ConversationEvent(event)) = reply.body else {
        panic!("expected an event")
    };
    event
}
fn error_message(reply: pb::Envelope) -> String {
    let Some(Body::Error(value)) = reply.body else {
        panic!("expected explicit error")
    };
    value.message.unwrap()
}
fn decision(input: journal::Id) -> pb::Envelope {
    envelope(Body::ConversationQueueDecision(
        pb::ConversationQueueDecision {
            request_id: Some(vec![5; 16]),
            input_id: Some(input.to_vec()),
            action: Some(4),
            ..Default::default()
        },
    ))
}

/// WS-01: each owner state maps to its step; cancel, terminal, text and a
/// pending tool remove it.
#[test]
fn ws01_each_owner_state_maps_to_its_step() {
    for (helper, accepted, expected) in [
        (
            Some(HelperStep::Preparing),
            false,
            Some(Step::PreparingModel),
        ),
        (Some(HelperStep::Hello), false, Some(Step::StartingModel)),
        (None, false, None),
        (Some(HelperStep::Input), true, Some(Step::SendingInput)),
        (None, true, None),
        (Some(HelperStep::Running), true, Some(Step::ModelOutput)),
        // Combinations the owner never produces still show no step.
        (Some(HelperStep::Preparing), true, None),
        (Some(HelperStep::Running), false, None),
    ] {
        let fixture = owner_at(helper, accepted);
        let active = fixture.owner.active.as_ref().unwrap();
        assert_eq!(wait_step(active), expected, "{helper:?} {accepted}");
        let preparing = fixture.owner.preparing_step();
        if accepted {
            assert_eq!(preparing, None);
        } else {
            assert_eq!(preparing, expected.map(|step| (INPUT, step)));
        }
    }
    let mut fixture = owner_at(Some(HelperStep::Running), true);
    let active = fixture.owner.active.as_mut().unwrap();
    active.response.revise(0, "first text", 3).unwrap();
    assert_eq!(wait_step(active), None);
    active.response.revise(0, "", 4).unwrap();
    active.cancel = Some(journal::Cause::UserCancel);
    assert_eq!(wait_step(active), None);
    active.cancel = None;
    active.terminal = Some(terminal(journal::TerminalKind::Failed));
    assert_eq!(wait_step(active), None);
    // A pending tool shows its own row; the tool fixture keeps its call.
    let tool = tool_tests::Fixture::new();
    assert!(tool.owner.active.as_ref().unwrap().tool.is_some());
    assert_eq!(wait_step(tool.owner.active.as_ref().unwrap()), None);
    // Without a queued input there is no client-visible pre-acceptance step.
    let mut direct = owner_at(Some(HelperStep::Hello), false);
    direct.owner.active.as_mut().unwrap().queue_input = None;
    assert_eq!(direct.owner.preparing_step(), None);
    assert_eq!(direct.owner.live_step(), None);
}
fn terminal(kind: journal::TerminalKind) -> journal::TurnTerminal {
    journal::TurnTerminal {
        operation: OPERATION,
        generation: 1,
        kind,
        cause: journal::Cause::ProviderFailure,
        final_cursor: u64::MAX,
        usage_known: true,
        output_tokens: 0,
        text_offset: 0,
        text: String::new(),
    }
}

/// WS-02 and WS-04: a PENDING event names the step; a snapshot does not.
#[test]
fn ws02_pending_event_carries_the_step_and_text_removes_it() {
    let mut fixture = owner_at(Some(HelperStep::Running), true);
    let observe = || {
        envelope(Body::ConversationObserve(pb::ConversationObserve {
            operation_id: Some(OPERATION.to_vec()),
            after_cursor: Some(2),
            wait_ms: Some(0),
            text_offset: None,
            thinking_offset: None,
        }))
    };
    let pending = event(
        fixture
            .owner
            .observe(observe(), OPERATION, 2, 0, 0)
            .unwrap(),
    );
    assert_eq!(pending.kind, Some(1));
    assert_eq!(pending.waiting_step, Some(Step::ModelOutput as i32));
    fixture.owner.active.as_mut().unwrap().model =
        ModelOwner::step_test_fixture(Some(HelperStep::Input));
    let sending = event(
        fixture
            .owner
            .observe(observe(), OPERATION, 2, 0, 0)
            .unwrap(),
    );
    assert_eq!(sending.waiting_step, Some(Step::SendingInput as i32));
    fixture.owner.active.as_mut().unwrap().model =
        ModelOwner::step_test_fixture(Some(HelperStep::Running));
    let active = fixture.owner.active.as_mut().unwrap();
    active.response.revise(0, "partial", 3).unwrap();
    active.cursor = 3;
    let snapshot = event(
        fixture
            .owner
            .observe(observe(), OPERATION, 2, 0, 0)
            .unwrap(),
    );
    assert_eq!(snapshot.kind, Some(2));
    assert_eq!(snapshot.waiting_step, None);
    // An equal-cursor heartbeat after text names no step.
    let heartbeat = event(
        fixture
            .owner
            .observe(observe(), OPERATION, 3, 7, 0)
            .unwrap(),
    );
    assert_eq!(heartbeat.kind, Some(1));
    assert_eq!(heartbeat.waiting_step, None);
}

/// WS-03: action 4 holds only the input under preparation.
#[test]
fn ws03_hold_targets_only_the_input_under_preparation() {
    let now = Instant::now();
    let mut fixture = owner_at(Some(HelperStep::Hello), false);
    let reply = fixture.owner.request(decision([8; 16]), now).unwrap();
    assert_eq!(error_message(reply), "input_not_preparing");
    assert!(fixture.owner.hold_reasons.is_empty());
    // The fixture has no writer: routing reaches the journal and records the
    // current-owner reason without fabricating a commit.
    let reply = fixture.owner.request(decision(INPUT), now).unwrap();
    assert_eq!(error_message(reply), "storage_unavailable");
    assert_eq!(fixture.owner.hold_reasons.get(&INPUT), Some(&"cancelled"));
    // After acceptance the turn uses ConversationCancel instead.
    let mut accepted = owner_at(Some(HelperStep::Input), true);
    let reply = accepted.owner.request(decision(INPUT), now).unwrap();
    assert_eq!(error_message(reply), "input_not_preparing");
    // A preparation already ending is not held twice.
    let mut ending = owner_at(Some(HelperStep::Hello), false);
    ending.owner.active.as_mut().unwrap().terminal = Some(terminal(journal::TerminalKind::Failed));
    let reply = ending.owner.request(decision(INPUT), now).unwrap();
    assert_eq!(error_message(reply), "input_not_preparing");
}

/// WS-05: a step change answers parked observations in the same poll; an
/// unchanged step leaves them parked. No timer is armed.
#[test]
fn ws05_step_change_answers_parked_observations() {
    let now = Instant::now();
    let later = now + Duration::from_secs(3600);
    let mut fixture = owner_at(Some(HelperStep::Hello), false);
    let owner = &mut fixture.owner;
    owner.observations.push((
        envelope(Body::ConversationQueueList(pb::ConversationQueueList {
            project_id: Some(vec![3; 16]),
            input_id: None,
            after_revision: Some(0),
        })),
        later,
    ));
    owner.publish_step(now);
    assert_eq!(
        owner.published_step,
        Some((false, INPUT, Step::StartingModel as i32))
    );
    assert!(owner.step_refresh);
    assert_eq!(owner.observations[0].1, now);
    owner.step_refresh = false;
    owner.observations[0].1 = later;
    owner.publish_step(now);
    assert!(!owner.step_refresh);
    assert_eq!(owner.observations[0].1, later);
    // A live pre-acceptance step makes the paced reply a full projection
    // read, not the empty pending stub. The fixture has no writer.
    owner.observations[0].1 = now;
    owner.observations(now);
    assert_eq!(
        error_message(owner.out.pop_front().unwrap()),
        "storage_unavailable"
    );
    // Without a step the paced reply is the existing pending stub.
    owner.active = None;
    owner.publish_step(now);
    owner.step_refresh = false;
    owner.observations.push((
        envelope(Body::ConversationQueueList(pb::ConversationQueueList {
            project_id: Some(vec![3; 16]),
            input_id: None,
            after_revision: Some(0),
        })),
        now,
    ));
    owner.observations(now);
    let Some(Body::ConversationQueueReply(stub)) = owner.out.pop_front().unwrap().body else {
        panic!("expected the pending stub")
    };
    assert_eq!(stub.pending, Some(true));
    assert!(stub.entries.is_empty());
}

/// WS-03: the committed Hold cancels only the matching preparation, once, and
/// records that the person held it so wind-down cannot hold it again.
#[test]
fn ws03_committed_hold_cancels_the_preparation_once() {
    let now = Instant::now();
    let mut fixture = owner_at(Some(HelperStep::Hello), false);
    let active = fixture.owner.active.as_mut().unwrap();
    Owner::person_hold(active, [8; 16], now);
    assert!(!active.person_held);
    assert!(active.terminal.is_none());
    assert_eq!(wait_step(active), Some(Step::StartingModel));
    Owner::person_hold(active, INPUT, now);
    assert!(active.person_held);
    assert_eq!(active.failure_reason, Some("cancelled"));
    let terminal = active.terminal.as_ref().unwrap();
    assert_eq!(terminal.kind, journal::TerminalKind::Cancelled);
    assert_eq!(terminal.cause, journal::Cause::UserCancel);
    // The helper is winding down: no step, and no second cancel.
    assert_eq!(active.model.helper_step(), None);
    assert_eq!(wait_step(active), None);
    // An accepted turn is cancelled through ConversationCancel instead.
    let mut accepted = owner_at(Some(HelperStep::Input), true);
    let active = accepted.owner.active.as_mut().unwrap();
    Owner::person_hold(active, INPUT, now);
    assert!(!active.person_held);
    assert!(active.terminal.is_none());
}
