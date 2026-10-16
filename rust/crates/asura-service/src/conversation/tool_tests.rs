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
            composition: None,
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
            person_held: false,
            request: None,
            turn,
            input: vec![],
            candidates: Vec::new(),
            model: ModelOwner::tool_test_fixture(),
            accepted: true,
            ready: false,
            start_committed: true,
            measurements: TurnMeasurements::default(),
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
                shell: None,
                secret_digest: None,
                memory_ticket: None,
                rejected: false,
                call,
                deadline: Some(now + Duration::from_secs(2)),
                intent_committed: false,
                dispatched: false,
                result: None,
            }),
            tool_budget: budget,
            tool_executor: tools::Executor::default(),
            shell_worker: crate::shell_worker::Worker::default(),
            tool_cache: vec![],
            secret_key: zeroize::Zeroizing::new([3; 32]),
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
                calibration: Default::default(),
                measurements: VecDeque::new(),
                hold_reasons: BTreeMap::new(),
                published_step: None,
                step_refresh: false,
                wake: None,
                observations: vec![],
                payloads: Default::default(),
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
            active.tool.as_mut().unwrap().deadline = Some(Instant::now());
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
fn retained_foreground_requests_wait_without_a_timer_and_drain_on_shutdown() {
    let mut fixture = Fixture::new();
    let request = pb::Envelope {
        service_epoch: Some(vec![1; 16]),
        attachment_id: Some(vec![2; 16]),
        request_counter: Some(1),
        body: Some(Body::ProjectList(pb::ProjectList {
            after_project_id: None,
            limit: Some(16),
        })),
    };
    let (ticket, _controller) =
        asura_storage::authority::writer::test_support::controlled_ticket(false);
    fixture.owner.pending = Some((ticket, Job::SensorLoad([3; 16])));
    fixture.owner.foreground.push_back(request);
    // J1-0: a queued command never expires while the journal ticket is pending.
    let later = Instant::now() + Duration::from_secs(3600);
    fixture.owner.dispatch_foreground(later);
    assert_eq!(fixture.owner.foreground.len(), 1);
    assert!(fixture.owner.out.is_empty());
    fixture.owner.closing = true;
    fixture.owner.dispatch_foreground(later);
    assert!(fixture.owner.foreground.is_empty());
    assert!(
        matches!(fixture.owner.out.pop_front().unwrap().body, Some(Body::ProjectsReply(value)) if value.error.as_deref() == Some("service_draining"))
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
            shell: None,
            secret_digest: None,
            memory_ticket: None,
            rejected: true,
            call,
            deadline: Some(now + Duration::from_secs(2)),
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
        assert_eq!(active.tool_cache[0].2.status, Some(3));
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
    tool.deadline = Some(now);
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
    active.tool.as_mut().unwrap().deadline = Some(now + Duration::from_secs(1));
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

pub(super) fn memory_reply() -> writer::Reply {
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
    let (ticket, controller) = writer::test_support::controlled_ticket(false);
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
            .tool_test_has_delivery(active.tool_cache[0].2.clone())
    );
}
#[test]
fn unsettled_memory_timeout_or_steering_never_delivers_late_body() {
    for steering in [false, true] {
        let mut fixture = Fixture::new();
        let now = Instant::now();
        let (ticket, controller) = writer::test_support::controlled_ticket(false);
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
        tool.deadline = Some(if steering {
            now + Duration::from_secs(2)
        } else {
            now
        });
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
    let mut replay = journal::replay(&bytes, &std::sync::atomic::AtomicBool::new(false)).unwrap();
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
            5 => tool.deadline = Some(Instant::now()),
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
            arguments: String::new(),
            preview: None,
            truncated: None,
            exit_code: None,
            create: None,
            kind: 14,
            offset: 0,
            limit: 0,
            result_status: None,
            intent_frame: 0..1,
            result_frame: None,
            result_bytes: 0,
            shell: None,
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

#[test]
fn turn_measurements_preserve_partial_counts_zero_and_frozen_duration() {
    let start = Instant::now();
    let mut m = TurnMeasurements {
        started: Some(start),
        ..Default::default()
    };
    let initial = m.snapshot(start).unwrap();
    assert_eq!(initial.input_tokens, None);
    assert_eq!(initial.complete, Some(false));
    m.record(&mp::InferenceUsage {
        request_ordinal: Some(1),
        input_tokens: Some(20),
        output_tokens: Some(0),
        complete: Some(true),
        ..Default::default()
    });
    m.record(&mp::InferenceUsage {
        request_ordinal: Some(2),
        input_tokens: None,
        output_tokens: Some(3),
        complete: Some(false),
        ..Default::default()
    });
    let partial = m.snapshot(start).unwrap();
    assert_eq!(partial.input_tokens, Some(20));
    assert_eq!(partial.output_tokens, Some(3));
    assert_eq!(partial.cached_input_tokens, None);
    assert_eq!(partial.complete, Some(false));
    m.ended = Some(start + std::time::Duration::from_millis(42));
    assert_eq!(
        m.snapshot(start + std::time::Duration::from_secs(10))
            .unwrap()
            .elapsed_ms,
        Some(42)
    );
    assert!(TurnMeasurements::default().snapshot(start).is_none());
}

#[test]
fn aggregate_measurements_fail_closed_on_arithmetic_overflow() {
    let mut m = TurnMeasurements {
        input: Some(u64::MAX),
        output: Some(0),
        ..Default::default()
    };
    m.record(&mp::InferenceUsage {
        request_ordinal: Some(1),
        input_tokens: Some(1),
        output_tokens: Some(0),
        complete: Some(true),
        ..Default::default()
    });
    assert_eq!(m.input, None);
    assert!(!m.complete);
}

#[test]
fn settled_measurements_cache_evicts_oldest_and_restart_stays_unknown() {
    let mut fixture = inventory_fixture();
    let mut active = fixture.owner.active.take().unwrap();
    for n in 1..=9 {
        active.turn.operation = [n; 16];
        fixture.owner.active = Some(active);
        fixture.acknowledge(Job::Finish);
        active = inventory_fixture().owner.active.take().unwrap();
    }
    assert_eq!(fixture.owner.measurements.len(), 8);
    assert_eq!(fixture.owner.measurements.front().unwrap().0, [2; 16]);
    assert_eq!(fixture.owner.measurements.back().unwrap().0, [9; 16]);
    assert!(inventory_fixture().owner.measurements.is_empty());
}

fn shell_call_of(active: &Active, command: &str) -> tools::Call {
    tools::Call {
        operation: active.turn.operation,
        generation: active.turn.generation,
        ordinal: 1,
        arguments: tools::Arguments::Shell {
            command: command.into(),
            cwd: ".".into(),
            timeout_seconds: 30,
        },
    }
}
fn shell_token() -> String {
    ["ghp_", &"xV7".repeat(12)].concat()
}
fn shell_text(call: &tools::Call) -> String {
    match &call.arguments {
        tools::Arguments::Shell { command, cwd, .. } => format!("{command}\0{cwd}"),
        _ => String::new(),
    }
}

/// J1T-03 (owner layer): a rewrite over 8,192 bytes is refused at
/// `ToolCall`, before any record; kind 22 encodes within its frame, so the
/// append cannot fail on size and make the service unavailable.
#[test]
fn j1t_03_overlong_rewrite_is_a_recorded_refusal() {
    let mut fixture = Fixture::new();
    let now = Instant::now();
    let active = fixture.owner.active.as_mut().unwrap();
    active.tool = None;
    active.tool_budget = tools::Budget::new(now + Duration::from_secs(60));
    let unit = ["curl -u admin:", "Sup3r", "S3cret", " x;"].concat();
    let long = format!(
        "{}echo {}",
        unit.repeat(10),
        "f".repeat(8192 - 60 - unit.len() * 10 - 5 + 1)
    );
    assert!(long.len() <= 8192);
    let call = shell_call_of(active, &long);
    Owner::tool_call(active, call, now, &fixture.owner.payloads, Some([9; 16]));
    let tool = active.tool.as_ref().expect("live tool");
    let shell = tool.shell.as_ref().expect("shell");
    assert_eq!(shell.intent.disposition, journal::ShellDisposition::Refused);
    assert_eq!(
        shell.intent.refusal.map(|r| r.code),
        Some(journal::RefusalCode::RewrittenCommandTooLong)
    );
    assert!(tool.rejected);
    assert_eq!(tool.result.as_ref().map(|r| r.status), Some(3));
    let context = journal::FrameContext {
        installation_id: [9; 16],
        transition_id: [1; 16],
        sequence: 2,
        prior_digest: [0; 32],
        expected_revision: 1,
        owner_generation: 1,
    };
    let frame = journal::encode_frame(
        &context,
        &journal::Record::ShellIntent(Box::new(shell.intent.clone())),
    )
    .expect("kind 22 encodes");
    assert!(frame.len() <= 9568);
    assert!(fixture.owner.unavailable.is_none());
}

/// J1T-04 (owner layer, memory inspection): after `ToolCall` and after the
/// result is cached, neither `LiveTool` nor `tool_cache` holds the raw
/// command; a repeat with a different value conflicts, an equal one matches.
#[test]
fn j1t_04_live_tool_and_cache_hold_no_raw_command() {
    let mut fixture = Fixture::new();
    let now = Instant::now();
    let token = shell_token();
    let raw = format!("printf '%s' \"{token}\" | wc -c");
    {
        let active = fixture.owner.active.as_mut().unwrap();
        active.tool = None;
        active.tool_budget = tools::Budget::new(now + Duration::from_secs(60));
        let call = shell_call_of(active, &raw);
        // No payload store is ready: the call is recorded busy, which
        // carries the rewrite too.
        Owner::tool_call(active, call, now, &fixture.owner.payloads, Some([9; 16]));
        let tool = active.tool.as_ref().unwrap();
        assert!(!shell_text(&tool.call).contains(&token));
        assert!(shell_text(&tool.call).contains("${ASURA_SECRET_1}"));
        let shell = tool.shell.as_ref().unwrap();
        assert_eq!(
            shell.intent.busy,
            Some(journal::ShellBusyReason::PayloadUnavailable)
        );
        assert!(!shell.intent.command.contains(&token));
        assert!(shell.secrets.is_empty(), "busy keeps no secret value");
        assert!(tool.secret_digest.is_some());
    }
    fixture.acknowledge(Job::ToolIntent);
    fixture.owner.unavailable = None;
    {
        let tool = fixture
            .owner
            .active
            .as_mut()
            .unwrap()
            .tool
            .as_mut()
            .unwrap();
        tool.dispatched = true;
    }
    fixture.acknowledge(Job::ToolResult);
    let active = fixture.owner.active.as_mut().unwrap();
    assert_eq!(active.tool_cache.len(), 1);
    let (cached, digest, wire) = &active.tool_cache[0];
    assert!(!shell_text(cached).contains(&token));
    assert!(digest.is_some());
    assert!(!wire.text.as_deref().unwrap_or("").contains(&token));
    // A repeated ordinal is compared by rewrite and keyed digest: the same
    // value matches the cache entry; a different value with the same rewrite
    // does not, and the owner treats it as a protocol fault.
    let other = ["ghp_", &"qQ2".repeat(12)].concat();
    let mut same = shell_call_of(active, &raw);
    let (_, same_digest) = shell_call::prepare(&mut same, &active.secret_key);
    assert_eq!(&same, cached);
    assert_eq!(Some(same_digest), *digest);
    let mut different = shell_call_of(active, &format!("printf '%s' \"{other}\" | wc -c"));
    let (_, different_digest) = shell_call::prepare(&mut different, &active.secret_key);
    assert_eq!(
        &different, cached,
        "the rewrite alone cannot tell them apart"
    );
    assert_ne!(Some(different_digest), *digest);
    let different = shell_call_of(active, &format!("printf '%s' \"{other}\" | wc -c"));
    Owner::tool_call(
        active,
        different,
        now,
        &fixture.owner.payloads,
        Some([9; 16]),
    );
    assert_eq!(active.failure_reason, Some("model_protocol_fault"));
}

/// J1T-05 unit (J1-Q3): a shell call has no tool deadline, so a result
/// committed long after it started never fails the turn as `model_timeout`;
/// the ordinary tool deadline (L10) is unchanged.
#[test]
fn j1t_05_model_timeout_never_applies_to_shell() {
    let mut fixture = Fixture::new();
    let now = Instant::now();
    {
        let active = fixture.owner.active.as_mut().unwrap();
        active.tool = None;
        active.tool_budget = tools::Budget::new(now + Duration::from_secs(3600));
        let call = shell_call_of(active, "sleep 65");
        Owner::tool_call(active, call, now, &fixture.owner.payloads, Some([9; 16]));
        assert!(active.tool.as_ref().unwrap().deadline.is_none());
    }
    fixture.acknowledge(Job::ToolIntent);
    {
        let tool = fixture
            .owner
            .active
            .as_mut()
            .unwrap()
            .tool
            .as_mut()
            .unwrap();
        assert!(tool.deadline.is_none(), "kind 22 commit sets no deadline");
        tool.dispatched = true;
    }
    fixture.owner.job_done(
        Job::ToolResult,
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
        now + Duration::from_secs(120),
    );
    let active = fixture.owner.active.as_ref().unwrap();
    assert_ne!(active.failure_reason, Some("model_timeout"));
    assert_eq!(active.tool_cache.len(), 1);
}
