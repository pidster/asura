//! Deterministic writer-acknowledgement boundaries; no helper or writer is spawned.
use super::*;
use std::{os::unix::fs::DirBuilderExt, time::Duration};

pub(super) struct Fixture {
    pub(super) owner: Owner,
    pub(super) path: std::path::PathBuf,
}
impl Fixture {
    pub(super) fn new() -> Self {
        let path = std::path::PathBuf::from(format!(
            "/private/tmp/asura-tool-boundary-{:x}",
            u128::from_ne_bytes(asura_platform::random_id())
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        let runtime = RuntimeDirectory::scratch(&path, true).unwrap();
        let now = Instant::now();
        let turn = journal::TurnAccepted {
            request: [1; 16],
            digest: [2; 32],
            project: [3; 16],
            original_conversation: None,
            expected_generation: 0,
            conversation: [5; 16],
            generation: 1,
            task: [6; 16],
            operation: [4; 16],
            model: "system".into(),
            configuration_digest: [7; 32],
            instructions_digest: [8; 32],
            input_digest: [9; 32],
            prior_operations: vec![],
            prompt: "observe status".into(),
            reserved_output_tokens: 512,
            event_cursor: 1,
        };
        let call = tools::Call {
            operation: turn.operation,
            generation: 1,
            ordinal: 1,
            arguments: tools::Arguments::ObserveStatus,
        };
        let mut budget = tools::Budget::new(now + Duration::from_secs(60));
        budget
            .reserve(
                call.clone(),
                &tool_grant(&turn, true),
                &tools::Destination::Local,
                now,
            )
            .unwrap();
        let active = Active {
            foreground_user: true,
            queue_input: None,
            request: None,
            turn,
            input: vec![],
            model: ModelOwner::tool_test_fixture(),
            accepted: true,
            ready: false,
            start_committed: true,
            tools_enabled: true,
            capabilities: crate::model::CapabilityProfile {
                supported: Some(1),
                source: Some(1),
                reasoning_disabled: false,
            },
            activation: [
                crate::model::Activation::Enabled,
                crate::model::Activation::Disabled,
                crate::model::Activation::Disabled,
                crate::model::Activation::Disabled,
            ],
            helper: [10; 16],
            snapshot: String::new(),
            cursor: 2,
            cancel: None,
            cancel_replies: vec![],
            terminal: None,
            failure_reason: None,
            tool: Some(LiveTool {
                create: None,
                shell_intent: None,
                memory_ticket: None,
                rejected: false,
                call,
                deadline: now + Duration::from_secs(2),
                intent_committed: false,
                dispatched: false,
                result: None,
            }),
            tool_budget: budget,
            tool_executor: tools::Executor::default(),
            shell_worker: crate::shell_worker::Worker::default(),
            tool_cache: vec![],
        };
        Self {
            path,
            owner: Owner {
                create_recovery: None,
                audit_records: Arc::new(Vec::new()),
                audit_health: audit_tools::unavailable(),
                audit_emitter: None,
                sensor_owner: None,
                sensor_scan_revision: None,
                sensor_scan_cursor: None,
                sensor_scan_pending: false,
                sensor_latest: BTreeMap::new(),
                sensor_epoch: [0; 16],
                sensor_reconcile: None,
                sensor_retried: BTreeSet::new(),
                sensor_failed: BTreeSet::new(),
                sensor_sequences: BTreeMap::new(),
                runtime,
                package: None,
                writer: None,
                pending: None,
                replay: None,
                active: Some(active),
                out: VecDeque::new(),
                unavailable: None,
                closing: false,
                started: true,
                automatic_initialization: AutomaticInitialization::Complete,
                model_context: VecDeque::new(),
                hold_reasons: BTreeMap::new(),
                wake: None,
                observations: vec![],
                foreground: VecDeque::new(),
                dispatching_foreground: false,
            },
        }
    }
    fn acknowledge(&mut self, job: Job) {
        self.owner.job_done(
            job,
            Ok(writer::Reply {
                memory_create_result: None,
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
            }),
            Instant::now(),
        );
    }
    fn stage_result(&mut self) -> journal::ToolResult {
        let tool = self.owner.active.as_mut().unwrap().tool.as_mut().unwrap();
        tool.intent_committed = true;
        tool.dispatched = true;
        let result = journal::ToolResult {
            operation: [4; 16],
            generation: 1,
            ordinal: 1,
            status: 1,
            text: "persisted status".into(),
            next_offset: None,
            truncated: false,
        };
        tool.result = Some(result.clone());
        result
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[test]
fn cancellation_after_intent_produces_failed_result_without_capture_or_delivery() {
    let mut fixture = Fixture::new();
    fixture.acknowledge(Job::ToolIntent);
    let active = fixture.owner.active.as_mut().unwrap();
    active.cancel = Some(journal::Cause::UserCancel);
    active.tool.as_mut().unwrap().dispatched = true;
    fixture.owner.capture_sensor_tool(Instant::now());
    let active = fixture.owner.active.as_ref().unwrap();
    let result = active.tool.as_ref().unwrap().result.as_ref().unwrap();
    assert_eq!(result.status, 6);
    assert!(result.text.is_empty());
    assert!(fixture.owner.sensor_owner.is_none());
    assert!(
        !active
            .model
            .tool_test_has_delivery(tool_wire_result(result))
    );
    assert!(active.tool_cache.is_empty());
}

#[test]
fn result_acknowledgement_after_cancel_or_deadline_never_delivers() {
    for expired in [false, true] {
        let mut fixture = Fixture::new();
        let result = fixture.stage_result();
        let active = fixture.owner.active.as_mut().unwrap();
        if expired {
            active.tool.as_mut().unwrap().deadline = Instant::now();
        } else {
            active.cancel = Some(journal::Cause::UserCancel);
        }
        fixture.acknowledge(Job::ToolResult);
        let active = fixture.owner.active.as_ref().unwrap();
        assert!(
            !active
                .model
                .tool_test_has_delivery(tool_wire_result(&result))
        );
        assert_eq!(
            active.tool_cache.len(),
            1,
            "acknowledged result stays available for durable replay"
        );
        assert!(active.tool_budget.is_settled());
        assert!(active.tool.is_none());
    }
}

#[test]
fn unconfirmed_intent_or_result_never_authorizes_delivery_or_cache() {
    for result_stage in [false, true] {
        let mut fixture = Fixture::new();
        let result = fixture.stage_result();
        let job = if result_stage {
            Job::ToolResult
        } else {
            fixture
                .owner
                .active
                .as_mut()
                .unwrap()
                .tool
                .as_mut()
                .unwrap()
                .intent_committed = false;
            Job::ToolIntent
        };
        fixture
            .owner
            .job_done(job, Err(writer::Error::OutcomeUnconfirmed), Instant::now());
        let active = fixture.owner.active.as_ref().unwrap();
        assert!(fixture.owner.unavailable.is_some());
        assert!(active.tool_cache.is_empty());
        assert!(!active.tool_budget.is_settled());
        assert!(
            !active
                .model
                .tool_test_has_delivery(tool_wire_result(&result))
        );
    }
}

#[test]
fn successful_result_requires_acknowledgement_before_delivery() {
    let mut fixture = Fixture::new();
    let result = fixture.stage_result();
    assert!(
        !fixture
            .owner
            .active
            .as_ref()
            .unwrap()
            .model
            .tool_test_has_delivery(tool_wire_result(&result))
    );
    fixture.acknowledge(Job::ToolResult);
    let active = fixture.owner.active.as_ref().unwrap();
    assert!(
        active
            .model
            .tool_test_has_delivery(tool_wire_result(&result))
    );
    assert!(active.tool_budget.is_settled());
}

#[test]
fn registry_refresh_waits_for_sensor_writer_then_runs_before_background_work() {
    let _writer_owner = crate::TEST_WRITER
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let mut fixture = Fixture::new();
    let owner = &mut fixture.owner;
    owner.active = None;
    owner.writer = Some(writer::WriterHandle::start(owner.runtime.clone()).unwrap());
    let ticket = owner
        .writer
        .as_ref()
        .unwrap()
        .try_submit(writer::Command::SensorLoad { project: [3; 16] })
        .unwrap();
    owner.pending = Some((ticket, Job::SensorLoad([3; 16])));
    let now = Instant::now();
    let request = pb::Envelope {
        service_epoch: Some(vec![1; 16]),
        attachment_id: Some(vec![2; 16]),
        request_counter: Some(1),
        body: Some(Body::ProjectList(pb::ProjectList {
            after_project_id: None,
            limit: Some(16),
        })),
    };
    assert!(owner.request(request.clone(), now).is_none());
    assert_eq!(owner.foreground.len(), 1);
    assert!(owner.out.is_empty());
    // Completion is injected independently of thread scheduling. The original
    // worker is still serialized; the next command is queued to that same owner.
    owner.pending = None;
    owner.dispatch_foreground(now);
    assert!(owner.foreground.is_empty());
    assert!(matches!(owner.pending, Some((_, Job::Projects(_)))));
    assert!(owner.out.is_empty());
    owner.pending = None;
    owner.writer.as_ref().unwrap().close();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !owner.writer.as_mut().unwrap().settled() && Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert!(owner.writer.as_mut().unwrap().settled());
}

#[test]
fn retained_foreground_requests_expire_without_storage_admission() {
    let mut fixture = Fixture::new();
    let now = Instant::now();
    let request = pb::Envelope {
        service_epoch: Some(vec![1; 16]),
        attachment_id: Some(vec![2; 16]),
        request_counter: Some(1),
        body: Some(Body::ProjectList(pb::ProjectList {
            after_project_id: None,
            limit: Some(16),
        })),
    };
    fixture.owner.foreground.push_back((request, now));
    fixture.owner.dispatch_foreground(now);
    assert!(fixture.owner.pending.is_none());
    assert!(fixture.owner.foreground.is_empty());
    assert!(
        matches!(fixture.owner.out.pop_front().unwrap().body, Some(Body::ProjectsReply(value)) if value.error.as_deref() == Some("storage_timeout"))
    );
}

#[test]
fn status_capture_does_not_return_before_sensor_commit() {
    let mut fixture = Fixture::new();
    let now = Instant::now();
    let wall = sensor_wall_ms();
    let mut owner = sensors::Owner::new(
        [2; 16],
        sensors::StatusSnapshot {
            lifecycle: 1,
            installation: 6,
            reason: 16,
        },
    )
    .unwrap();
    owner
        .restore(sensor_record::ProjectState::empty([3; 16]), now, wall)
        .unwrap();
    fixture.owner.sensor_owner = Some(owner);
    let tool = fixture
        .owner
        .active
        .as_mut()
        .unwrap()
        .tool
        .as_mut()
        .unwrap();
    tool.intent_committed = true;
    tool.dispatched = true;
    fixture.owner.capture_sensor_tool(now);
    assert!(
        fixture
            .owner
            .active
            .as_ref()
            .unwrap()
            .tool
            .as_ref()
            .unwrap()
            .result
            .is_none()
    );
    let owner = fixture.owner.sensor_owner.as_mut().unwrap();
    let write = owner.take_write().unwrap();
    owner
        .persisted([3; 16], write.state.write_id, Ok(()))
        .unwrap();
    fixture.owner.capture_sensor_tool(now);
    let result = fixture
        .owner
        .active
        .as_ref()
        .unwrap()
        .tool
        .as_ref()
        .unwrap()
        .result
        .as_ref()
        .unwrap();
    assert_eq!(result.status, 1);
    assert!(result.text.contains("observation_id"));
    assert!(result.text.contains("expires_ms"));
    assert!(
        !fixture
            .owner
            .active
            .as_ref()
            .unwrap()
            .model
            .tool_test_has_delivery(tool_wire_result(result))
    );
}

#[test]
fn helper_settlement_cancellation_preserves_first_service_failure() {
    for (message, cause) in [
        ("model_protocol_fault", journal::Cause::ProtocolFailure),
        ("model_timeout", journal::Cause::Deadline),
    ] {
        let mut fixture = Fixture::new();
        let active = fixture.owner.active.as_mut().unwrap();
        active.snapshot = "retained partial response".into();
        Owner::failed(active, message, Instant::now());
        let original = active.terminal.clone().unwrap();
        assert_eq!(original.cause, cause);
        Owner::helper_terminal(active, 3, 7, Some(1));
        assert_eq!(active.terminal.as_ref(), Some(&original));
        Owner::failed(active, "later_provider_failure", Instant::now());
        assert_eq!(active.terminal.as_ref(), Some(&original));
    }
}

#[test]
fn helper_settlement_preserves_explicit_cancellation_cause() {
    let mut fixture = Fixture::new();
    let active = fixture.owner.active.as_mut().unwrap();
    active.cancel = Some(journal::Cause::UserCancel);
    Owner::helper_terminal(active, 3, 7, None);
    let original = active.terminal.clone().unwrap();
    assert_eq!(original.kind, journal::TerminalKind::Cancelled);
    assert_eq!(original.cause, journal::Cause::UserCancel);
    Owner::helper_terminal(active, 2, 8, Some(1));
    Owner::failed(active, "model_protocol_fault", Instant::now());
    assert_eq!(active.terminal.as_ref(), Some(&original));
}

#[test]
fn invalid_semantic_path_is_durable_rejection_not_host_dispatch() {
    for read in [false, true] {
        let mut fixture = Fixture::new();
        let now = Instant::now();
        let active = fixture.owner.active.as_mut().unwrap();
        let arguments = if read {
            mp::tool_call::Arguments::ReadFile(mp::ProjectReadFile {
                path: Some("../outside".into()),
                offset: Some(u64::MAX),
                limit: Some(16),
            })
        } else {
            mp::tool_call::Arguments::ListDirectory(mp::ProjectListDirectory {
                path: Some("/private".into()),
            })
        };
        let call = proposed_tool(
            &active.turn,
            mp::ToolCall {
                ordinal: Some(1),
                arguments: Some(arguments),
            },
        )
        .unwrap();
        active.tool_budget = tools::Budget::new(now + Duration::from_secs(60));
        active
            .tool_budget
            .reserve_rejected(
                call.clone(),
                &tool_grant(&active.turn, active.foreground_user),
                &tools::Destination::Local,
                now,
            )
            .unwrap();
        let result = tool_result(
            &call,
            Err(tools::ExecutionError::Rejected(
                tools::Rejection::InvalidArguments,
            )),
        );
        assert_eq!(result.status, 3);
        assert!(result.text.is_empty());
        assert_eq!(tool_intent(&call, true).kind, if read { 4 } else { 5 });
        active.tool = Some(LiveTool {
            create: None,
            shell_intent: None,
            memory_ticket: None,
            rejected: true,
            call,
            deadline: now + Duration::from_secs(2),
            intent_committed: false,
            dispatched: false,
            result: Some(result.clone()),
        });
        assert!(
            !active
                .model
                .tool_test_has_delivery(tool_wire_result(&result))
        );
        fixture.acknowledge(Job::ToolIntent);
        // No writer is installed: the attempted result commit fails closed. No
        // executor may run despite a committed intent and a live operation.
        fixture.owner.drive(now);
        let active = fixture.owner.active.as_ref().unwrap();
        assert!(active.tool_executor.is_settled());
        assert!(
            !active
                .model
                .tool_test_has_delivery(tool_wire_result(&result))
        );
        fixture.owner.unavailable = None;
        fixture.acknowledge(Job::ToolResult);
        let active = fixture.owner.active.as_ref().unwrap();
        assert!(
            active
                .model
                .tool_test_has_delivery(tool_wire_result(&result))
        );
        assert_eq!(active.tool_cache[0].1.status, Some(3));
        assert!(active.tool_budget.is_settled());
    }
}

#[test]
fn cached_rejection_replays_only_exact_current_proposal() {
    let fixture = Fixture::new();
    let turn = &fixture.owner.active.as_ref().unwrap().turn;
    let call = tools::Call {
        operation: turn.operation,
        generation: turn.generation,
        ordinal: 1,
        arguments: tools::Arguments::ListDirectory {
            path: "../outside".into(),
        },
    };
    assert_eq!(
        call.arguments.validate(),
        Err(tools::Rejection::InvalidArguments)
    );
    assert!(validate_cached_tool(&call, &call, turn, true).is_ok());
    let mut changed = call.clone();
    changed.arguments = tools::Arguments::ListDirectory { path: "..".into() };
    assert_eq!(
        validate_cached_tool(&call, &changed, turn, true,),
        Err(tools::Rejection::IdentityConflict)
    );
    let mut stale = turn.clone();
    stale.generation += 1;
    assert_eq!(
        validate_cached_tool(&call, &call, &stale, true,),
        Err(tools::Rejection::Stale)
    );
}

#[test]
fn memory_intent_kinds_are_explicit_and_rejections_never_dispatch() {
    let mut fixture = Fixture::new();
    let active = fixture.owner.active.as_mut().unwrap();
    for (arguments, valid, rejected) in [
        (
            tools::Arguments::MemoryListNotes {
                after: None,
                limit: 1,
            },
            6,
            9,
        ),
        (
            tools::Arguments::MemoryGetNote {
                version: "ab".repeat(16),
                offset: 0,
                limit: 2,
            },
            7,
            10,
        ),
        (
            tools::Arguments::MemoryNoteSources {
                version: "ab".repeat(16),
            },
            8,
            11,
        ),
    ] {
        let call = tools::Call {
            operation: active.turn.operation,
            generation: active.turn.generation,
            ordinal: 1,
            arguments,
        };
        assert_eq!(tool_intent(&call, false).kind, valid);
        assert_eq!(tool_intent(&call, true).kind, rejected);
    }
    let tool = active.tool.as_mut().unwrap();
    tool.call.arguments = tools::Arguments::MemoryListNotes {
        after: None,
        limit: 0,
    };
    tool.rejected = true;
    tool.intent_committed = true;
    tool.result = Some(tool_result(
        &tool.call,
        Err(tools::ExecutionError::Rejected(
            tools::Rejection::InvalidArguments,
        )),
    ));
    fixture.owner.drive(Instant::now());
    let tool = fixture
        .owner
        .active
        .as_ref()
        .unwrap()
        .tool
        .as_ref()
        .unwrap();
    assert!(tool.memory_ticket.is_none());
    assert_eq!(tool.result.as_ref().unwrap().status, 3);
}
#[test]
fn memory_cancel_and_deadline_suppress_late_data_and_preserve_slot() {
    let mut fixture = Fixture::new();
    let now = Instant::now();
    let active = fixture.owner.active.as_mut().unwrap();
    let tool = active.tool.as_mut().unwrap();
    tool.call.arguments = tools::Arguments::MemoryGetNote {
        version: "ab".repeat(16),
        offset: 0,
        limit: 16,
    };
    tool.intent_committed = true;
    tool.dispatched = true;
    tool.deadline = now;
    fixture.owner.capture_memory_tool(now);
    let active = fixture.owner.active.as_mut().unwrap();
    assert_eq!(
        active
            .tool
            .as_ref()
            .unwrap()
            .result
            .as_ref()
            .unwrap()
            .status,
        5
    );
    assert!(
        active
            .tool_budget
            .reserve(
                active.tool.as_ref().unwrap().call.clone(),
                &tool_grant(&active.turn, active.foreground_user),
                &tools::Destination::Local,
                now
            )
            .is_err()
    );
    active.cancel = Some(journal::Cause::UserCancel);
    active.tool.as_mut().unwrap().deadline = now + Duration::from_secs(1);
    fixture.owner.capture_memory_tool(now);
    let result = fixture
        .owner
        .active
        .as_ref()
        .unwrap()
        .tool
        .as_ref()
        .unwrap()
        .result
        .as_ref()
        .unwrap();
    assert_eq!(result.status, 6);
    assert!(result.text.is_empty());
}

fn memory_reply() -> writer::Reply {
    let id = |n| asura_storage::memory::Id::new([n; 16]).unwrap();
    writer::Reply {
        memory_create_result: None,
        memory_result: Some(Ok(asura_storage::memory::ReadResult::Note(
            asura_storage::memory::Note {
                binding: asura_storage::memory::Binding {
                    installation_id: id(1),
                    graph_id: id(2),
                    init_operation_id: id(3),
                },
                context_id: id(3),
                object_id: id(5),
                version_id: id(6),
                operation_id: id(7),
                body: "late secret".into(),
                body_sha256: [8; 32],
            },
        ))),
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
#[test]
fn memory_response_does_not_release_slot_before_worker_settles() {
    let mut fixture = Fixture::new();
    let now = Instant::now();
    let (ticket, controller) =
        writer::test_support::controlled_ticket(now + Duration::from_secs(20), false);
    let active = fixture.owner.active.as_mut().unwrap();
    let tool = active.tool.as_mut().unwrap();
    tool.call.arguments = tools::Arguments::MemoryGetNote {
        version: "06".repeat(16),
        offset: 0,
        limit: 16,
    };
    tool.intent_committed = true;
    tool.dispatched = true;
    tool.memory_ticket = Some(ticket);
    active.tool_budget = tools::Budget::cancellation_driven();
    active
        .tool_budget
        .reserve(
            tool.call.clone(),
            &tool_grant(&active.turn, active.foreground_user),
            &tools::Destination::Local,
            now,
        )
        .unwrap();
    assert!(controller.publish(Ok(memory_reply())));
    fixture.owner.capture_memory_tool(now);
    fixture.owner.drive(now);
    let active = fixture.owner.active.as_ref().unwrap();
    let tool = active.tool.as_ref().unwrap();
    assert_eq!(tool.result.as_ref().unwrap().text, "late secret");
    assert!(!tool.memory_ticket.as_ref().unwrap().is_settled());
    assert!(fixture.owner.pending.is_none());
    assert!(fixture.owner.unavailable.is_none());
    assert!(
        !active
            .model
            .tool_test_has_delivery(tool_wire_result(tool.result.as_ref().unwrap()))
    );
    controller.finish();
    assert!(
        fixture
            .owner
            .active
            .as_ref()
            .unwrap()
            .tool
            .as_ref()
            .unwrap()
            .memory_ticket
            .as_ref()
            .unwrap()
            .is_settled()
    );
    // Simulate the existing writer's durable result acknowledgement only after settlement.
    fixture.acknowledge(Job::ToolResult);
    let active = fixture.owner.active.as_ref().unwrap();
    assert!(active.tool.is_none());
    assert_eq!(active.tool_cache.len(), 1);
    assert!(
        active
            .model
            .tool_test_has_delivery(active.tool_cache[0].1.clone())
    );
}
#[test]
fn unsettled_memory_timeout_or_steering_never_delivers_late_body() {
    for steering in [false, true] {
        let mut fixture = Fixture::new();
        let now = Instant::now();
        let (ticket, controller) =
            writer::test_support::controlled_ticket(now + Duration::from_secs(20), false);
        let active = fixture.owner.active.as_mut().unwrap();
        let tool = active.tool.as_mut().unwrap();
        tool.call.arguments = tools::Arguments::MemoryGetNote {
            version: "06".repeat(16),
            offset: 0,
            limit: 16,
        };
        tool.intent_committed = true;
        tool.dispatched = true;
        tool.memory_ticket = Some(ticket);
        tool.deadline = if steering {
            now + Duration::from_secs(2)
        } else {
            now
        };
        if steering {
            active.cancel = Some(journal::Cause::Steering);
        }
        if steering {
            install_recorded_memory_cancel(&mut fixture);
        }
        assert!(controller.publish(Ok(memory_reply())));
        fixture.owner.capture_memory_tool(now);
        fixture.owner.drive(now);
        let active = fixture.owner.active.as_ref().unwrap();
        let tool = active.tool.as_ref().unwrap();
        let result = tool.result.as_ref().unwrap();
        assert_eq!(result.status, if steering { 6 } else { 5 });
        assert!(result.text.is_empty());
        assert!(!tool.memory_ticket.as_ref().unwrap().is_settled());
        assert!(fixture.owner.pending.is_none());
        assert!(fixture.owner.unavailable.is_none());
        assert!(
            !active
                .model
                .tool_test_has_delivery(tool_wire_result(result))
        );
        assert!(active.tool_cache.is_empty());
        controller.finish();
        assert!(
            fixture
                .owner
                .active
                .as_ref()
                .unwrap()
                .tool
                .as_ref()
                .unwrap()
                .memory_ticket
                .as_ref()
                .unwrap()
                .is_settled()
        );
    }
}

fn install_recorded_memory_cancel(fixture: &mut Fixture) {
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
        Instant::now() + Duration::from_secs(2),
        &std::sync::atomic::AtomicBool::new(false),
    )
    .unwrap();
    let active = fixture.owner.active.as_ref().unwrap();
    replay.operations.insert(
        active.turn.operation,
        journal::Operation {
            reserved_output_tokens: 512,
            conversation: active.turn.conversation,
            generation: active.turn.generation,
            task: active.turn.task,
            owner_generation: 1,
            input_digest: active.turn.input_digest,
            accepted_frame: 0..1,
            helper: None,
            cancel: active.cancel,
            terminal: None,
            tools: vec![],
        },
    );
    fixture.owner.replay = Some(Arc::new(replay));
}

fn inventory_fixture() -> Fixture {
    let mut fixture = Fixture::new();
    let active = fixture.owner.active.as_mut().unwrap();
    let tool = active.tool.as_mut().unwrap();
    tool.call.arguments = tools::Arguments::ListTools;
    active.tool_budget = tools::Budget::cancellation_driven();
    active
        .tool_budget
        .reserve(
            tool.call.clone(),
            &tool_grant(&active.turn, active.foreground_user),
            &tools::Destination::Local,
            Instant::now(),
        )
        .unwrap();
    fixture
}
#[test]
fn inventory_requires_durable_intent_and_result_without_host_or_database_execution() {
    let mut fixture = inventory_fixture();
    fixture
        .owner
        .active
        .as_mut()
        .unwrap()
        .tool
        .as_mut()
        .unwrap()
        .dispatched = true;
    fixture.owner.capture_inventory_tool(Instant::now());
    assert!(
        fixture
            .owner
            .active
            .as_ref()
            .unwrap()
            .tool
            .as_ref()
            .unwrap()
            .result
            .is_none()
    );
    fixture.acknowledge(Job::ToolIntent);
    fixture.owner.capture_inventory_tool(Instant::now());
    let active = fixture.owner.active.as_ref().unwrap();
    let tool = active.tool.as_ref().unwrap();
    assert_eq!(tool_intent(&tool.call, false).kind, 14);
    let result = tool.result.clone().unwrap();
    assert_eq!(result.status, 1);
    assert_eq!(result.text, tools::inventory_text().unwrap());
    let now = Instant::now();
    assert!(
        fixture
            .owner
            .next_deadline(now)
            .is_some_and(|deadline| deadline <= now),
        "static completion must schedule durable publication without another external event"
    );
    assert!(active.tool_executor.is_settled());
    assert!(tool.memory_ticket.is_none());
    assert!(active.tool_cache.is_empty());
    assert!(
        !active
            .model
            .tool_test_has_delivery(tool_wire_result(&result))
    );
    fixture.acknowledge(Job::ToolResult);
    let active = fixture.owner.active.as_ref().unwrap();
    assert_eq!(active.tool_cache.len(), 1);
    assert!(
        active
            .model
            .tool_test_has_delivery(tool_wire_result(&result))
    );
}
#[test]
fn inventory_obeys_cancellation_deadline_and_disabled_tool_fences() {
    for status in [6, 5, 2] {
        let mut fixture = inventory_fixture();
        fixture.acknowledge(Job::ToolIntent);
        let active = fixture.owner.active.as_mut().unwrap();
        let tool = active.tool.as_mut().unwrap();
        tool.dispatched = true;
        match status {
            6 => active.cancel = Some(journal::Cause::UserCancel),
            5 => tool.deadline = Instant::now(),
            2 => active.tools_enabled = false,
            _ => unreachable!(),
        }
        fixture.owner.capture_inventory_tool(Instant::now());
        let active = fixture.owner.active.as_ref().unwrap();
        let result = active.tool.as_ref().unwrap().result.as_ref().unwrap();
        assert_eq!(result.status, status);
        assert!(result.text.is_empty());
        assert!(active.tool_cache.is_empty());
        assert!(active.tool_executor.is_settled());
    }
}

#[test]
fn committed_tool_activity_is_observable_before_model_text() {
    let mut fixture = inventory_fixture();
    let operation = fixture.owner.active.as_ref().unwrap().turn.operation;
    let before = fixture.owner.active.as_ref().unwrap().cursor;
    fixture.acknowledge(Job::ToolIntent);
    install_recorded_memory_cancel(&mut fixture);
    Arc::get_mut(fixture.owner.replay.as_mut().unwrap())
        .unwrap()
        .operations
        .get_mut(&operation)
        .unwrap()
        .tools
        .push(journal::ToolRecord {
            create: None,
            kind: 14,
            offset: 0,
            limit: 0,
            result_status: None,
            intent_frame: 0..1,
            result_frame: None,
            result_bytes: 0,
        });
    assert!(fixture.owner.active.as_ref().unwrap().cursor > before);
    assert!(fixture.owner.active.as_ref().unwrap().snapshot.is_empty());
    let reply = fixture
        .owner
        .observe(pb::Envelope::default(), operation, before)
        .unwrap();
    let Some(Body::ConversationEvent(event)) = reply.body else {
        panic!("activity event");
    };
    assert_eq!(event.kind, Some(1));
    assert!(event.text.is_none());
    assert_eq!(event.tools[0].name.as_deref(), Some("service_list_tools"));
    assert_eq!(event.tools[0].state, Some(1));
    Arc::get_mut(fixture.owner.replay.as_mut().unwrap())
        .unwrap()
        .operations
        .get_mut(&operation)
        .unwrap()
        .tools[0]
        .result_status = Some(1);
    let reply = fixture
        .owner
        .observe(pb::Envelope::default(), operation, before)
        .unwrap();
    let Some(Body::ConversationEvent(event)) = reply.body else {
        panic!("completed tool event");
    };
    assert_eq!(event.tools[0].state, Some(2));
    assert!(event.text.is_none());
}
