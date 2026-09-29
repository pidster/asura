use super::*;
use crate::conversation::tool_tests::Fixture;
use asura_storage::authority::writer::test_support::controlled_ticket;
use std::sync::atomic::Ordering;

fn intent() -> journal::MemoryCreateIntent {
    memory::prepare_create(
        memory::Binding {
            installation_id: memory::Id::new([1; 16]).unwrap(),
            graph_id: memory::Id::new([2; 16]).unwrap(),
            init_operation_id: memory::Id::new([3; 16]).unwrap(),
        },
        memory::Id::new([3; 16]).unwrap(),
        [4; 16],
        1,
        1,
        "remember me".into(),
        None,
    )
    .unwrap()
    .0
}
fn receipt(intent: &journal::MemoryCreateIntent) -> memory::Receipt {
    memory::Receipt {
        operation_id: memory::Id::new(intent.memory_operation).unwrap(),
        command_sha256: intent.command_sha256,
        version_id: memory::Id::new(intent.version).unwrap(),
        edge_id: None,
    }
}
fn response(outcome: MemoryCreateResult) -> writer::Reply {
    writer::Reply {
        memory_create_result: Some(Ok(outcome)),
        memory_result: None,
        sensor_state: None,
        replay: None,
        record: None,
        history: None,
        prepared: None,
        projects: vec![],
        inputs: vec![],
        queued: None,
        order_revision: None,
        stale_order: false,
        accepted_input_id: None,
        project_name: None,
        project_name_revision: None,
        project_name_changed: None,
        stale_project_name_revision: false,
    }
}
struct Writer(writer::WriterHandle);
impl Drop for Writer {
    fn drop(&mut self) {
        self.0.close();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.0.settled() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}
#[test]
fn resolution_response_is_not_publication_until_job_really_settles() {
    let _serial = crate::TEST_WRITER.lock().unwrap_or_else(|p| p.into_inner());
    let fixture = Fixture::new();
    let writer = Writer(writer::WriterHandle::start(fixture.owner.runtime.clone()).unwrap());
    let now = Instant::now();
    let mut mutation = Mutation::new(intent());
    mutation.phase = Phase::Resolve;
    let (ticket, controller) = controlled_ticket(now + Duration::from_secs(2), false);
    mutation.ticket = Some(ticket);
    controller.publish(Ok(response(MemoryCreateResult::Committed(receipt(
        &mutation.intent,
    )))));
    mutation.cancel();
    assert!(mutation.poll(&writer.0, now, 6).unwrap().is_none());
    assert!(!mutation.settled());
    controller.finish();
    let result = mutation.poll(&writer.0, now, 6).unwrap().unwrap();
    assert_eq!(result.status, 1);
    assert_eq!(result.text, mutation.intent.success_text());
    assert!(mutation.settled());
}
#[test]
fn timed_out_mutation_retains_ticket_and_cancellation_until_settlement() {
    let _serial = crate::TEST_WRITER.lock().unwrap_or_else(|p| p.into_inner());
    let fixture = Fixture::new();
    let writer = Writer(writer::WriterHandle::start(fixture.owner.runtime.clone()).unwrap());
    let now = Instant::now();
    let mut mutation = Mutation::new(intent());
    mutation.phase = Phase::Mutation;
    let (ticket, controller) = controlled_ticket(now, true);
    mutation.ticket = Some(ticket);
    mutation.cancel();
    assert!(mutation.poll(&writer.0, now, 5).unwrap().is_none());
    assert!(mutation.cancel.load(Ordering::Acquire));
    assert!(!mutation.settled());
    assert!(mutation.next_deadline().is_none());
    controller.publish(Ok(response(MemoryCreateResult::Committed(receipt(
        &mutation.intent,
    )))));
    assert!(mutation.poll(&writer.0, now, 5).unwrap().is_none());
    assert!(mutation.phase == Phase::Mutation);
    controller.finish();
    assert!(mutation.poll(&writer.0, now, 5).unwrap().is_none());
    assert!(mutation.phase == Phase::Resolve);
}
#[test]
fn read_authority_and_background_provenance_cannot_authorize_create() {
    let fixture = Fixture::new();
    let turn = &fixture.owner.active.as_ref().unwrap().turn;
    let call = tools::Call {
        operation: turn.operation,
        generation: turn.generation,
        ordinal: 1,
        arguments: tools::Arguments::MemoryCreateNote {
            body: "note".into(),
            source_version: None,
        },
    };
    assert_eq!(
        tools::validate(&call, &tool_grant(turn, false), &tools::Destination::Local),
        Err(tools::Rejection::Denied)
    );
    assert!(tools::validate(&call, &tool_grant(turn, true), &tools::Destination::Local).is_ok());
    let mut grant = tool_grant(turn, true);
    grant.remote_destination = Some("approved".into());
    assert_eq!(
        tools::validate(
            &call,
            &grant,
            &tools::Destination::Remote("approved".into())
        ),
        Err(tools::Rejection::Denied)
    );
}
#[test]
fn shutdown_revokes_pending_create_in_same_drive_iteration() {
    let mut fixture = Fixture::new();
    let tool = fixture
        .owner
        .active
        .as_mut()
        .unwrap()
        .tool
        .as_mut()
        .unwrap();
    let mutation = Mutation::new(intent());
    let cancel = mutation.cancel.clone();
    tool.create = Some(mutation);
    tool.intent_committed = true;
    tool.dispatched = true;
    fixture.owner.closing = true;
    fixture.owner.drive(Instant::now());
    assert!(cancel.load(Ordering::Acquire));
}

#[test]
fn acknowledged_committed_create_after_cancel_stays_success_but_is_not_delivered() {
    let mut fixture = Fixture::new();
    let now = Instant::now();
    let active = fixture.owner.active.as_mut().unwrap();
    let call = tools::Call {
        operation: active.turn.operation,
        generation: 1,
        ordinal: 1,
        arguments: tools::Arguments::MemoryCreateNote {
            body: "remember me".into(),
            source_version: None,
        },
    };
    let mut budget = tools::Budget::new(now + Duration::from_secs(60));
    budget
        .reserve(
            call.clone(),
            &tool_grant(&active.turn, true),
            &tools::Destination::Local,
            now,
        )
        .unwrap();
    active.tool_budget = budget;
    let mutation = Mutation::new(intent());
    let result = mutation.result(MemoryCreateResult::Committed(receipt(&mutation.intent)), 6);
    active.tool = Some(LiveTool {
        create: Some(mutation),
        shell_intent: None,
        rejected: false,
        call,
        deadline: now + Duration::from_secs(2),
        memory_ticket: None,
        intent_committed: true,
        dispatched: true,
        result: Some(result.clone()),
    });
    active.cancel = Some(journal::Cause::UserCancel);
    fixture.owner.job_done(
        Job::ToolResult,
        Ok(response(MemoryCreateResult::NotCommitted)),
        now,
    );
    let active = fixture.owner.active.as_ref().unwrap();
    assert_eq!(active.tool_cache[0].1.status, Some(1));
    assert_eq!(
        active.tool_cache[0].1.text.as_deref(),
        Some(result.text.as_str())
    );
    assert!(
        !active
            .model
            .tool_test_has_delivery(tool_wire_result(&result))
    );
}

fn pending_projection() -> Arc<journal::Replay> {
    let create = intent();
    let bytes = journal::encode_frame(
        &journal::FrameContext {
            installation_id: [1; 16],
            transition_id: [9; 16],
            sequence: 1,
            prior_digest: [0; 32],
            expected_revision: 0,
            owner_generation: 1,
        },
        &journal::Record::PendingInit(journal::PendingInit {
            request: [3; 16],
            graph: [2; 16],
            mode: 1,
            configuration_revision: 1,
            configuration_digest: [8; 32],
            digest: journal::request_digest(journal::Request::Initialize { mode: 1 }).unwrap(),
        }),
    )
    .unwrap();
    let mut replay = journal::replay(
        &bytes,
        Instant::now() + Duration::from_secs(2),
        &std::sync::atomic::AtomicBool::new(false),
    )
    .unwrap();
    replay.owner_generation = 2;
    replay.active_operation = Some(create.operation);
    replay.operations.insert(
        create.operation,
        journal::Operation {
            reserved_output_tokens: 2048,
            conversation: [5; 16],
            generation: 1,
            task: [6; 16],
            owner_generation: 1,
            input_digest: [7; 32],
            accepted_frame: 0..1,
            helper: Some([8; 16]),
            cancel: None,
            terminal: None,
            tools: vec![journal::ToolRecord {
                kind: 12,
                intent_frame: 0..1,
                create: Some(Box::new(create)),
                offset: 0,
                limit: 0,
                result_frame: None,
                result_status: None,
                result_bytes: 0,
            }],
        },
    );
    Arc::new(replay)
}
#[test]
fn startup_unconfirmed_resolution_retains_publication_gate_without_terminal_or_model() {
    let _serial = crate::TEST_WRITER.lock().unwrap_or_else(|p| p.into_inner());
    let mut fixture = ServiceFixture(Fixture::new());
    fixture.owner.active = None;
    fixture.owner.writer =
        Some(writer::WriterHandle::start(fixture.owner.runtime.clone()).unwrap());
    let mut opened = response(MemoryCreateResult::NotCommitted);
    opened.replay = Some(pending_projection());
    fixture
        .owner
        .job_done(Job::Open, Ok(opened), Instant::now());
    assert!(fixture.owner.create_recovery.is_some());
    assert!(
        fixture
            .owner
            .replay
            .as_ref()
            .unwrap()
            .project_memory_blocked([3; 16])
    );
    let (ticket, controller) = controlled_ticket(Instant::now() + Duration::from_secs(2), false);
    let recovery = fixture.owner.create_recovery.as_mut().unwrap();
    recovery.phase = Phase::Resolve;
    recovery.ticket = Some(ticket);
    controller.publish(Err(writer::Error::Unavailable));
    assert!(fixture.owner.drive_create_recovery(Instant::now()));
    assert!(
        fixture.owner.unavailable.is_none(),
        "response alone is not settlement"
    );
    controller.finish();
    assert!(fixture.owner.drive_create_recovery(Instant::now()));
    assert_eq!(
        fixture.owner.unavailable,
        Some("memory_write_outcome_unconfirmed")
    );
    assert!(fixture.owner.pending.is_none());
    assert!(fixture.owner.active.is_none());
    assert!(
        fixture.owner.replay.as_ref().unwrap().operations[&[4; 16]]
            .terminal
            .is_none()
    );
    assert!(
        fixture
            .owner
            .replay
            .as_ref()
            .unwrap()
            .project_memory_blocked([3; 16])
    );
    fixture.owner.writer.as_ref().unwrap().close();
    let end = Instant::now() + Duration::from_secs(10);
    while !fixture.owner.writer.as_mut().unwrap().settled() {
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(1));
    }
}

struct ServiceFixture(Fixture);
impl std::ops::Deref for ServiceFixture {
    type Target = Fixture;
    fn deref(&self) -> &Fixture {
        &self.0
    }
}
impl std::ops::DerefMut for ServiceFixture {
    fn deref_mut(&mut self) -> &mut Fixture {
        &mut self.0
    }
}
impl Drop for ServiceFixture {
    fn drop(&mut self) {
        if let Some(writer) = &self.owner.writer {
            writer.close();
        }
        let end = Instant::now() + Duration::from_secs(10);
        while self.owner.writer.as_mut().is_some_and(|w| !w.settled()) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

fn wait_writer(writer: &writer::WriterHandle, command: writer::Command) -> writer::Reply {
    let mut ticket = writer.try_submit(command).unwrap();
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(reply) = ticket.poll() {
            return reply.unwrap();
        }
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(1));
    }
}
#[test]
fn actual_startup_reconciles_receipt_then_persists_result_and_interruption_without_model() {
    let _serial = crate::TEST_WRITER.lock().unwrap_or_else(|p| p.into_inner());
    let mut fixture = ServiceFixture(Fixture::new());
    let runtime = fixture.owner.runtime.clone();
    let writer = Writer(writer::WriterHandle::start(runtime.clone()).unwrap());
    wait_writer(&writer.0, writer::Command::Initialize { request: [30; 16] });
    let registered = wait_writer(
        &writer.0,
        writer::Command::Register {
            request: [31; 16],
            location: fixture.path.to_str().unwrap().into(),
        },
    )
    .replay
    .unwrap();
    let project = *registered.projects.keys().next().unwrap();
    let revision = registered.revision;
    let binding = memory::Binding {
        installation_id: memory::Id::new(registered.installation_id).unwrap(),
        graph_id: memory::Id::new(registered.initialization.graph).unwrap(),
        init_operation_id: memory::Id::new(registered.initialization.request).unwrap(),
    };
    drop(registered);
    let prepared = wait_writer(
        &writer.0,
        writer::Command::Prepare {
            project,
            conversation: None,
            expected_generation: 0,
            prompt: "remember".into(),
        },
    )
    .prepared
    .unwrap();
    let mut turn = fixture.owner.active.as_ref().unwrap().turn.clone();
    turn.project = project;
    turn.prompt = "remember".into();
    turn.configuration_digest = prepared.configuration_digest;
    turn.reserved_output_tokens = 2048;
    turn.digest = journal::request_digest(journal::Request::Submit {
        project,
        conversation: None,
        expected_generation: 0,
        prompt: "remember",
    })
    .unwrap();
    let mut state = wait_writer(
        &writer.0,
        writer::Command::Append {
            expected_revision: revision,
            record: journal::Record::TurnAccepted(Box::new(turn.clone())),
            admission: Some(writer::AdmissionFence {
                project,
                configuration_digest: prepared.configuration_digest,
            }),
        },
    )
    .replay
    .unwrap();
    state = wait_writer(
        &writer.0,
        writer::Command::Append {
            expected_revision: state.revision,
            record: journal::Record::StartAuthorized(journal::StartAuthorized {
                operation: turn.operation,
                generation: 1,
                helper: [40; 16],
                input_digest: turn.input_digest,
            }),
            admission: None,
        },
    )
    .replay
    .unwrap();
    let (intent, _) = memory::prepare_create(
        binding,
        memory::Id::new(project).unwrap(),
        turn.operation,
        1,
        1,
        "restart evidence".into(),
        None,
    )
    .unwrap();
    let revision = state.revision;
    drop(state);
    wait_writer(
        &writer.0,
        writer::Command::Append {
            expected_revision: revision,
            record: journal::Record::MemoryCreateIntent(intent.clone()),
            admission: None,
        },
    );
    let outcome = wait_writer(
        &writer.0,
        writer::Command::MemoryCreate {
            turn: turn.operation,
            generation: 1,
            ordinal: 1,
            body: "restart evidence".into(),
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        },
    )
    .memory_create_result
    .unwrap()
    .unwrap();
    assert!(matches!(outcome, MemoryCreateResult::Committed(_)));
    drop(writer);
    // No ToolResult exists: production Open must reconcile before it may interrupt.
    fixture.owner = Owner::new(runtime, None);
    fixture.owner.open();
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        fixture.owner.poll(Instant::now());
        assert!(
            fixture.owner.unavailable.is_none(),
            "{:?}",
            fixture.owner.unavailable
        );
        assert!(
            fixture.owner.active.is_none(),
            "startup must not start inference"
        );
        if fixture.owner.replay.as_ref().is_some_and(|r| {
            r.operations
                .get(&turn.operation)
                .is_some_and(|op| op.terminal.is_some())
        }) {
            break;
        }
        assert!(Instant::now() < end, "startup reconciliation stalled");
        std::thread::sleep(Duration::from_millis(1));
    }
    let replay = fixture.owner.replay.as_ref().unwrap();
    let operation = &replay.operations[&turn.operation];
    assert_eq!(operation.tools[0].result_status, Some(1));
    assert_eq!(
        operation.terminal.as_ref().unwrap().kind,
        journal::TerminalKind::Interrupted
    );
    assert_eq!(operation.terminal.as_ref().unwrap().charged_tokens, 2048);
    assert!(!replay.project_memory_blocked(project));
    let result_frame = operation.tools[0].result_frame.clone().unwrap();
    fixture.owner.shutdown();
    let end = Instant::now() + Duration::from_secs(10);
    while !fixture.owner.settled() {
        fixture.owner.poll(Instant::now());
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(1));
    }
    let bytes = std::fs::read(fixture.path.join(".asura/state/control/slot-0.log")).unwrap();
    journal::replay(
        &bytes,
        Instant::now() + Duration::from_secs(2),
        &std::sync::atomic::AtomicBool::new(false),
    )
    .unwrap();
    let frame = &bytes[result_frame];
    // Format 1 frame boundaries, as in the native journal proof harness.
    match journal::decode_record(15, &frame[144..frame.len() - 40]).unwrap() {
        journal::Record::ToolResult(result) => assert_eq!(result.text, intent.success_text()),
        _ => panic!("missing durable create result"),
    }
}

#[test]
fn shutdown_revokes_create_before_another_reactor_iteration() {
    let mut fixture = Fixture::new();
    let mutation = Mutation::new(intent());
    let cancel = mutation.cancel.clone();
    fixture
        .owner
        .active
        .as_mut()
        .unwrap()
        .tool
        .as_mut()
        .unwrap()
        .create = Some(mutation);
    assert!(!cancel.load(Ordering::Acquire));
    fixture.owner.shutdown();
    assert!(cancel.load(Ordering::Acquire));
}

#[test]
fn failure_and_helper_terminal_revoke_create_at_entry_without_poll() {
    for helper_terminal in [false, true] {
        let mut fixture = Fixture::new();
        let mutation = Mutation::new(intent());
        let cancel = mutation.cancel.clone();
        let active = fixture.owner.active.as_mut().unwrap();
        active.tool.as_mut().unwrap().create = Some(mutation);
        assert!(!cancel.load(Ordering::Acquire));
        if helper_terminal {
            Owner::helper_terminal(active, 2, 9, None);
        } else {
            Owner::failed(active, "model_timeout", Instant::now());
        }
        assert!(cancel.load(Ordering::Acquire));
        assert!(active.terminal.is_some());
        // Duplicate lifecycle events must not bypass revocation on early return.
        let mutation = Mutation::new(intent());
        let cancel = mutation.cancel.clone();
        active.tool.as_mut().unwrap().create = Some(mutation);
        if helper_terminal {
            Owner::helper_terminal(active, 2, 9, None);
        } else {
            Owner::failed(active, "model_timeout", Instant::now());
        }
        assert!(cancel.load(Ordering::Acquire));
    }
}
