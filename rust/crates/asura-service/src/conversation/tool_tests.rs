//! Deterministic writer-acknowledgement boundaries. Most tests spawn no
//! helper or writer; the `Journaled` cases drive a real private journal.
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
            reserved_output_tokens: 2048,
            event_cursor: 1,
            composition: journal::Composition::default(),
        };
        let active = Active {
            foreground_user: true,
            queue_input: None,
            person_held: false,
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
            tool: None,
            tool_budget: tools::Budget::cancellation_driven(),
            tool_executor: tools::Executor::default(),
            exec_cmd_worker: crate::exec_cmd_worker::Worker::default(),
            tool_cache: vec![],
            secret_key: zeroize::Zeroizing::new([3; 32]),
        };
        let mut fixture = Self {
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
        };
        fixture.set_tool("service", r#"{"command":"status"}"#);
        fixture
    }
    /// Replaces the pending call with ordinal 1 of `name` and `arguments`,
    /// decided by the real decision path, and counts it in a fresh budget.
    pub(super) fn set_tool(&mut self, name: &str, arguments: &str) -> &mut LiveTool {
        let value = wire(1, name, arguments);
        let tool = decided(&self.owner, &value);
        encodes(&tool.record);
        let active = self.owner.active.as_mut().unwrap();
        active.tool_budget = tools::Budget::cancellation_driven();
        active.tool_budget.record(1).unwrap();
        active.tool = Some(tool);
        active.tool.as_mut().unwrap()
    }
    pub(super) fn active(&mut self) -> &mut Active {
        self.owner.active.as_mut().unwrap()
    }
    pub(super) fn acknowledge(&mut self, job: Job) {
        self.owner.job_done(
            job,
            Ok(writer::Reply {
                memory_create_result: None,
                memory_result: None,
                sensor_state: None,
                // A real reply carries the committed projection; keep the
                // installed one so later appends still name a revision.
                replay: self.owner.replay.clone(),
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
    pub(super) fn stage_result(&mut self) -> journal::ToolResult {
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

/// One received `ToolCall` as the helper sends it (source generated_content).
pub(super) fn wire(ordinal: u32, name: &str, arguments: &str) -> mp::ToolCall {
    mp::ToolCall {
        ordinal: Some(ordinal),
        name: Some(name.into()),
        arguments: Some(arguments.into()),
        arguments_bytes: None,
        name_bytes: None,
        source: Some(1),
    }
}
/// The real decision for `value` in the fixture's turn.
pub(super) fn decided(owner: &Owner, value: &mp::ToolCall) -> LiveTool {
    let active = owner.active.as_ref().unwrap();
    let digest = received_digest(&active.secret_key, value);
    Owner::decide(
        active,
        value,
        digest,
        &owner.payloads,
        owner.replay.as_deref(),
        asura_platform::secrets::detection(),
    )
}
/// Every kind 14 a test produces must be accepted by the real codec.
pub(super) fn encodes(record: &journal::ToolCall) -> Vec<u8> {
    journal::encode_frame(
        &journal::FrameContext {
            installation_id: [9; 16],
            transition_id: [1; 16],
            sequence: 2,
            prior_digest: [0; 32],
            expected_revision: 1,
            owner_generation: 1,
        },
        &journal::Record::ToolCall(Box::new(record.clone())),
    )
    .unwrap_or_else(|error| panic!("kind 14 must encode: {error:?} {record:?}"))
}
pub(super) fn get_note(prefix: &str) -> String {
    format!(
        "{{\"command\":\"get_note\",\"version\":\"{}\",\"limit\":16}}",
        prefix.repeat(16)
    )
}
/// Submits `value` through the owner's `ToolCall` path, as the helper does.
fn propose(fixture: &mut Fixture, value: mp::ToolCall) {
    let owner = &mut fixture.owner;
    let replay = owner.replay.clone();
    let active = owner.active.as_mut().unwrap();
    active.model.tool_test_pending(value.ordinal.unwrap());
    Owner::tool_call(
        active,
        value,
        Instant::now(),
        &owner.payloads,
        replay.as_deref(),
    );
}
fn fresh_turn(fixture: &mut Fixture) {
    let active = fixture.active();
    active.tool = None;
    active.tool_budget = tools::Budget::cancellation_driven();
    active.tool_cache.clear();
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
fn result_acknowledgement_after_cancel_never_delivers() {
    for cause in [journal::Cause::UserCancel, journal::Cause::Steering] {
        let mut fixture = Fixture::new();
        let result = fixture.stage_result();
        let active = fixture.owner.active.as_mut().unwrap();
        active.cancel = Some(cause);
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
        // The committed result bytes are accounted.
        assert_eq!(
            active.tool_budget.result_capacity(),
            tools::MAX_RESULT_BYTES.min(tools::MAX_TOTAL_RESULT_BYTES - result.text.len())
        );
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
            fixture.active().tool.as_mut().unwrap().intent_committed = false;
            Job::ToolIntent
        };
        fixture
            .owner
            .job_done(job, Err(writer::Error::OutcomeUnconfirmed), Instant::now());
        let active = fixture.owner.active.as_ref().unwrap();
        assert!(fixture.owner.unavailable.is_some());
        assert!(active.tool_cache.is_empty());
        // The call stays pending and no result bytes are spent.
        assert!(active.tool.is_some());
        assert_eq!(
            active.tool_budget.result_capacity(),
            tools::MAX_RESULT_BYTES
        );
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
            .active()
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
    assert!(active.tool.is_none());
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
        ("model_unavailable", journal::Cause::ProviderFailure),
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

/// The results queued for the helper so far.
fn sent(fixture: &mut Fixture) -> Vec<mp::ToolResult> {
    fixture.active().model.tool_test_results()
}
fn sent_result(ordinal: u32, status: i32, text: &str) -> mp::ToolResult {
    mp::ToolResult {
        ordinal: Some(ordinal),
        status: Some(status),
        text: Some(text.into()),
        next_offset: None,
        truncated: Some(false),
    }
}
fn pending_record(fixture: &mut Fixture) -> journal::ToolCall {
    let record = fixture
        .active()
        .tool
        .as_ref()
        .expect("pending call")
        .record
        .clone();
    encodes(&record);
    record
}

/// TCR-I01 (owner part): `project` `read_file` without `path` is a kind 14
/// record with `invalid_arguments: missing_field` and the stored text. It
/// commits, gets no kind 15, sends status 3 and the turn continues: the next
/// ordinal runs.
#[test]
fn tcr_i01_missing_field_is_a_recorded_rejection_and_the_turn_continues() {
    let mut fixture = Fixture::new();
    fresh_turn(&mut fixture);
    let text = r#"{"command":"read_file"}"#;
    propose(&mut fixture, wire(1, "project", text));
    let record = pending_record(&mut fixture);
    assert_eq!(record.decision, journal::ToolDecision::InvalidArguments);
    assert_eq!(record.fault, Some(journal::ToolFault::MissingField));
    assert_eq!(
        record.tool_operation,
        Some(journal::ToolOperation::ReadFile)
    );
    assert_eq!(record.form, journal::ArgumentForm::Verbatim);
    assert_eq!(
        (record.name.as_str(), record.arguments.as_str()),
        ("project", text)
    );
    assert_eq!(record.arguments_bytes as usize, text.len());
    assert!(fixture.active().tool.as_ref().unwrap().call.is_none());
    // Nothing is sent before the record commits.
    assert!(sent(&mut fixture).is_empty());
    fixture.acknowledge(Job::ToolIntent);
    assert_eq!(
        sent(&mut fixture),
        vec![sent_result(1, 3, "invalid_arguments: missing_field")]
    );
    let active = fixture.active();
    assert!(active.tool.is_none(), "complete at kind 14: no kind 15");
    assert!(active.tool_executor.is_settled());
    assert!(active.terminal.is_none() && active.failure_reason.is_none());
    assert_eq!(active.tool_cache.len(), 1);
    assert_eq!(active.tool_budget.next_ordinal(), 2);
    assert_eq!(
        active.tool_budget.result_capacity(),
        tools::MAX_RESULT_BYTES
    );
    // No kind 15 append is attempted for the rejected call.
    fixture.owner.drive(Instant::now());
    assert!(fixture.owner.pending.is_none());
    assert!(fixture.owner.unavailable.is_none());
    // The model may correct itself within the call limit.
    propose(
        &mut fixture,
        wire(2, "project", r#"{"command":"list_directory"}"#),
    );
    let tool = fixture.active().tool.as_ref().unwrap();
    assert_eq!(tool.record.decision, journal::ToolDecision::Run);
    assert_eq!(
        tool.call.as_ref().map(|c| c.arguments.clone()),
        Some(tools::Arguments::ListDirectory { path: ".".into() })
    );
    assert!(fixture.active().failure_reason.is_none());
}

/// Semantic path faults are recorded rejections; no executor ever starts.
#[test]
fn invalid_semantic_path_is_durable_rejection_not_host_dispatch() {
    for text in [
        r#"{"command":"read_file","path":"../outside","offset":9223372036854775000,"limit":16}"#,
        r#"{"command":"list_directory","path":"/private"}"#,
    ] {
        let mut fixture = Fixture::new();
        fresh_turn(&mut fixture);
        propose(&mut fixture, wire(1, "project", text));
        let record = pending_record(&mut fixture);
        assert_eq!(record.decision, journal::ToolDecision::InvalidArguments);
        assert!(matches!(
            record.fault,
            Some(journal::ToolFault::OutOfRange | journal::ToolFault::UnsafePath)
        ));
        fixture.acknowledge(Job::ToolIntent);
        fixture.owner.drive(Instant::now());
        let active = fixture.active();
        assert!(active.tool_executor.is_settled());
        assert!(active.tool.is_none());
        assert_eq!(active.tool_cache[0].2.status, Some(3));
    }
}

/// Decision table: unknown tool, denied create and the two `limit` causes.
#[test]
fn unknown_denied_and_limit_decisions_render_from_codes() {
    // unknown_tool
    let mut fixture = Fixture::new();
    fresh_turn(&mut fixture);
    propose(
        &mut fixture,
        wire(1, "delete_everything", r#"{"path":"/"}"#),
    );
    let record = pending_record(&mut fixture);
    assert_eq!(record.decision, journal::ToolDecision::UnknownTool);
    assert_eq!(record.fault, Some(journal::ToolFault::UnregisteredName));
    assert_eq!(record.tool_operation, None);
    assert_eq!(record.name, "delete_everything");
    fixture.acknowledge(Job::ToolIntent);
    assert_eq!(
        sent(&mut fixture),
        vec![sent_result(1, 3, "unknown_tool: unregistered_name")]
    );
    // denied: create_note without a foreground person
    let mut fixture = Fixture::new();
    fresh_turn(&mut fixture);
    fixture.active().foreground_user = false;
    propose(
        &mut fixture,
        wire(1, "memory", r#"{"command":"create_note","body":"note"}"#),
    );
    let record = pending_record(&mut fixture);
    assert_eq!(record.decision, journal::ToolDecision::Denied);
    assert_eq!(
        record.tool_operation,
        Some(journal::ToolOperation::CreateNote)
    );
    assert!(fixture.active().tool.as_ref().unwrap().create.is_none());
    fixture.acknowledge(Job::ToolIntent);
    assert_eq!(sent(&mut fixture), vec![sent_result(1, 2, "denied")]);
    // limit: ordinal 9 is recorded only as `limit`
    let mut fixture = Fixture::new();
    fresh_turn(&mut fixture);
    for ordinal in 1..=8 {
        fixture.active().tool_budget.record(ordinal).unwrap();
    }
    propose(&mut fixture, wire(9, "service", r#"{"command":"status"}"#));
    let record = pending_record(&mut fixture);
    assert_eq!(
        (record.ordinal, record.decision),
        (9, journal::ToolDecision::Limit)
    );
    fixture.acknowledge(Job::ToolIntent);
    assert_eq!(sent(&mut fixture), vec![sent_result(9, 7, "limit")]);
    // limit: result bytes used up; the turn continues
    let mut fixture = Fixture::new();
    fresh_turn(&mut fixture);
    for ordinal in 1..=4 {
        let budget = &mut fixture.active().tool_budget;
        budget.record(ordinal).unwrap();
        budget.settle(tools::MAX_RESULT_BYTES).unwrap();
    }
    propose(&mut fixture, wire(5, "service", r#"{"command":"status"}"#));
    assert_eq!(
        pending_record(&mut fixture).decision,
        journal::ToolDecision::Limit
    );
    fixture.acknowledge(Job::ToolIntent);
    assert_eq!(sent(&mut fixture), vec![sent_result(5, 7, "limit")]);
    assert!(fixture.active().terminal.is_none());
    // limit: create_note needs 256 bytes of result capacity
    let mut fixture = Fixture::new();
    fresh_turn(&mut fixture);
    let budget = &mut fixture.active().tool_budget;
    budget.record(1).unwrap();
    for _ in 0..3 {
        budget.settle(tools::MAX_RESULT_BYTES).unwrap();
    }
    budget.settle(tools::MAX_RESULT_BYTES - 100).unwrap();
    propose(
        &mut fixture,
        wire(2, "memory", r#"{"command":"create_note","body":"note"}"#),
    );
    assert_eq!(
        pending_record(&mut fixture).decision,
        journal::ToolDecision::Limit
    );
}

/// Refused exec_cmd: the SL3 refusal is recorded in fields 21-23 with the
/// display-redacted command; no record field or result holds the value.
#[test]
fn refused_exec_cmd_records_refusal_codes_and_no_value() {
    let mut fixture = Fixture::new();
    fresh_turn(&mut fixture);
    let token = exec_cmd_token();
    let text = serde_json::json!({ "command": format!("echo '{token}'") }).to_string();
    propose(&mut fixture, wire(1, "exec_cmd", &text));
    let record = pending_record(&mut fixture);
    assert_eq!(record.decision, journal::ToolDecision::Refused);
    let refusal = record.refusal.expect("refusal fields");
    assert_eq!(
        refusal.code,
        asura_platform::secrets::RefusalCode::SecretSingleQuoted
    );
    assert_eq!(
        refusal.kind,
        Some(asura_platform::secrets::SecretKind::GithubToken)
    );
    assert_eq!(record.form, journal::ArgumentForm::Canonical);
    assert!(record.arguments.contains("$ASURA_SECRET_1"));
    assert!(!format!("{record:?}").contains(&token));
    assert!(fixture.active().tool.as_ref().unwrap().exec_cmd.is_none());
    fixture.acknowledge(Job::ToolIntent);
    let results = sent(&mut fixture);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].status, Some(3));
    let rendered = results[0].text.clone().unwrap();
    assert!(rendered.starts_with("Command refused: a `github-token` secret"));
    assert!(!rendered.contains(&token));
    assert!(!format!("{:?}", fixture.active().tool_cache).contains(&token));
}

/// Every operation that runs is dispatched only after its kind 14 commits.
#[test]
fn run_calls_dispatch_only_after_the_record_commits() {
    let version = "ab".repeat(16);
    for (name, text) in [
        (
            "project",
            r#"{"command":"read_file","path":"README.md"}"#.to_string(),
        ),
        ("project", r#"{"command":"list_directory"}"#.into()),
        ("service", r#"{"command":"status"}"#.into()),
        ("service", r#"{"command":"tools"}"#.into()),
        ("service", r#"{"command":"audit"}"#.into()),
        ("memory", r#"{"command":"list_notes"}"#.into()),
        (
            "memory",
            format!(r#"{{"command":"get_note","version":"{version}"}}"#),
        ),
        (
            "memory",
            format!(r#"{{"command":"note_sources","version":"{version}"}}"#),
        ),
    ] {
        let mut fixture = Fixture::new();
        fresh_turn(&mut fixture);
        propose(&mut fixture, wire(1, name, &text));
        let record = pending_record(&mut fixture);
        assert_eq!(record.decision, journal::ToolDecision::Run, "{text}");
        // The append fails here (no writer): nothing may dispatch.
        fixture.owner.drive(Instant::now());
        let active = fixture.active();
        let tool = active.tool.as_ref().unwrap();
        assert!(!tool.intent_committed && !tool.dispatched, "{text}");
        assert!(tool.result.is_none() && tool.memory_ticket.is_none());
        assert!(active.tool_executor.is_settled());
        fixture.owner.unavailable = None;
        fixture.acknowledge(Job::ToolIntent);
        assert!(
            sent(&mut fixture).is_empty(),
            "a run call sends its kind 15 result only"
        );
        fixture.owner.drive(Instant::now());
        assert!(
            fixture.active().tool.as_ref().unwrap().dispatched,
            "{text} dispatches after the commit"
        );
    }
}

/// TCR-I10: a resend at a used ordinal with the same text gets the same
/// result again; different text is a protocol fault with one record only.
#[test]
fn tcr_i10_resend_is_compared_by_keyed_digest() {
    let mut fixture = Fixture::new();
    fresh_turn(&mut fixture);
    let first = wire(1, "project", r#"{"command":"read_file"}"#);
    propose(&mut fixture, first.clone());
    // An equal resend while pending is not a second call.
    propose(&mut fixture, first.clone());
    assert!(fixture.active().failure_reason.is_none());
    fixture.acknowledge(Job::ToolIntent);
    propose(&mut fixture, first.clone());
    let results = sent(&mut fixture);
    assert_eq!(results.len(), 2);
    assert_eq!(results[0], results[1]);
    assert!(fixture.active().failure_reason.is_none());
    assert!(fixture.active().tool.is_none(), "no second record");
    let mut changed = first;
    changed.arguments = Some(r#"{"command":"read_file","path":"a"}"#.into());
    propose(&mut fixture, changed);
    let active = fixture.active();
    assert_eq!(active.failure_reason, Some("model_protocol_fault"));
    assert!(active.tool.is_none());
    assert_eq!(active.tool_cache.len(), 1);
    // A different source or size is also a different call.
    let mut fixture = Fixture::new();
    fresh_turn(&mut fixture);
    let call = wire(1, "service", r#"{"command":"status"}"#);
    propose(&mut fixture, call.clone());
    let mut other = call;
    other.source = Some(2);
    propose(&mut fixture, other);
    assert_eq!(
        fixture.active().failure_reason,
        Some("model_protocol_fault")
    );
}

/// TCR-D7: a gap ordinal and a second call while one is pending are helper
/// faults with no record.
#[test]
fn tcr_d7_gap_and_concurrent_calls_are_helper_faults_without_record() {
    let mut fixture = Fixture::new();
    fresh_turn(&mut fixture);
    propose(&mut fixture, wire(2, "service", r#"{"command":"status"}"#));
    assert_eq!(
        fixture.active().failure_reason,
        Some("model_protocol_fault")
    );
    assert!(fixture.active().tool.is_none());
    let mut fixture = Fixture::new();
    fresh_turn(&mut fixture);
    propose(&mut fixture, wire(1, "service", r#"{"command":"status"}"#));
    propose(&mut fixture, wire(2, "service", r#"{"command":"tools"}"#));
    let active = fixture.active();
    assert_eq!(active.failure_reason, Some("model_protocol_fault"));
    assert_eq!(active.tool.as_ref().unwrap().ordinal, 1);
    assert_eq!(active.tool_budget.next_ordinal(), 2);
}

/// TCR-U10: when the rule set failed its self-check, every call keeps no
/// text and no name, and is refused (cancelled or limit keep their order).
#[test]
fn tcr_u10_rule_set_unavailable_retains_nothing_and_refuses() {
    use asura_platform::secrets::SelfCheckError;
    for (name, text) in [
        ("project", r#"{"command":"read_file","path":"a"}"#),
        ("memory", r#"{"command":"list_notes"}"#),
        ("service", r#"{"command":"status"}"#),
        ("exec_cmd", r#"{"command":"printf ok"}"#),
        ("not_registered", "{"),
    ] {
        for (ordinal, cancelled, expected) in [
            (1, false, journal::ToolDecision::Refused),
            (9, false, journal::ToolDecision::Limit),
            (1, true, journal::ToolDecision::Cancelled),
        ] {
            let mut fixture = Fixture::new();
            fresh_turn(&mut fixture);
            if cancelled {
                fixture.active().cancel = Some(journal::Cause::UserCancel);
            }
            let value = wire(ordinal, name, text);
            let active = fixture.owner.active.as_ref().unwrap();
            let tool = Owner::decide(
                active,
                &value,
                [7; 32],
                &fixture.owner.payloads,
                None,
                Err(SelfCheckError::Set),
            );
            let record = &tool.record;
            assert_eq!(record.decision, expected, "{name} {ordinal} {cancelled}");
            assert_eq!(
                record.form,
                journal::ArgumentForm::NotRetained(
                    journal::NotRetainedReason::DetectionUnavailable
                )
            );
            assert!(record.name.is_empty() && record.arguments.is_empty());
            assert_eq!(
                (record.rule_set_revision, record.rule_set_digest),
                (0, [0; 32])
            );
            assert_eq!(record.tool_operation, None);
            assert_eq!(record.arguments_bytes as usize, text.len());
            assert!(tool.call.is_none() && tool.exec_cmd.is_none());
            encodes(record);
            if expected == journal::ToolDecision::Refused {
                assert_eq!(
                    tools::not_run_result(record.decision, None, None, None),
                    Some((3, "refused: secret_detection_unavailable".into()))
                );
            }
        }
    }
}

/// TCR-U16: a 16,384-byte body of ordinary text, newlines and quotes is
/// stored in full; a body of control characters whose JSON exceeds 64,000
/// bytes arrives as a size only and is recorded as oversized.
#[test]
fn tcr_u16_create_note_body_sizes() {
    let mut fixture = Fixture::new();
    install_recorded_memory_cancel(&mut fixture);
    fresh_turn(&mut fixture);
    let unit = "line \"quoted\"\n";
    let mut body = unit.repeat(16_384 / unit.len());
    body.push_str(&"x".repeat(16_384 - body.len()));
    assert_eq!(body.len(), 16_384);
    let text = serde_json::json!({"command": "create_note", "body": body}).to_string();
    assert!(text.len() <= 64_000);
    propose(&mut fixture, wire(1, "memory", &text));
    let record = pending_record(&mut fixture);
    assert_eq!(record.decision, journal::ToolDecision::Run);
    assert_eq!(record.form, journal::ArgumentForm::Verbatim);
    assert_eq!(record.arguments, text, "stored in full");
    let stored: serde_json::Value = serde_json::from_str(&record.arguments).unwrap();
    assert_eq!(stored["body"].as_str(), Some(body.as_str()));
    // Control characters: 16,384 × `\u0001` is 98,304 bytes of JSON, so the
    // helper sends only the size (known-size exhaustion).
    let control =
        serde_json::json!({"command": "create_note", "body": "\u{1}".repeat(16_384)}).to_string();
    assert!(control.len() > 64_000);
    let mut fixture = Fixture::new();
    fresh_turn(&mut fixture);
    let mut value = wire(1, "memory", "");
    value.arguments = None;
    value.arguments_bytes = Some(control.len() as u32);
    propose(&mut fixture, value);
    let record = pending_record(&mut fixture);
    assert_eq!(record.decision, journal::ToolDecision::InvalidArguments);
    assert_eq!(record.fault, Some(journal::ToolFault::ArgumentsOverLimit));
    assert_eq!(
        record.form,
        journal::ArgumentForm::NotRetained(journal::NotRetainedReason::Oversized)
    );
    assert_eq!(record.arguments_bytes as usize, control.len());
    assert!(record.arguments.is_empty());
    fixture.acknowledge(Job::ToolIntent);
    assert_eq!(
        sent(&mut fixture),
        vec![sent_result(1, 3, "invalid_arguments: arguments_over_limit")]
    );
}

/// TCR-U13 (service part): the live create identity equals the replay
/// derivation from the same binding and call.
#[test]
fn tcr_u13_live_create_identity_equals_replay_derivation() {
    let mut fixture = Fixture::new();
    install_recorded_memory_cancel(&mut fixture);
    fresh_turn(&mut fixture);
    propose(
        &mut fixture,
        wire(
            1,
            "memory",
            &format!(
                r#"{{"command":"create_note","body":"b","source_version":"{}"}}"#,
                "ab".repeat(16)
            ),
        ),
    );
    let replay = fixture.owner.replay.clone().unwrap();
    let active = fixture.active();
    let tool = active.tool.as_ref().unwrap();
    assert_eq!(tool.record.decision, journal::ToolDecision::Run);
    let live = tool.create.as_ref().unwrap().intent;
    let derived = journal::CreateNoteCall::derive(
        [
            replay.installation_id,
            replay.initialization.graph,
            replay.initialization.request,
        ],
        active.turn.project,
        active.turn.operation,
        active.turn.generation,
        1,
    )
    .unwrap();
    assert_eq!(live, derived);
}
#[test]
fn stalled_memory_read_waits_without_a_timer_and_ends_on_cancel() {
    let mut fixture = Fixture::new();
    fixture.set_tool("memory", &get_note("ab"));
    let (ticket, controller) = writer::test_support::controlled_ticket(false);
    let active = fixture.owner.active.as_mut().unwrap();
    let tool = active.tool.as_mut().unwrap();
    tool.intent_committed = true;
    tool.dispatched = true;
    tool.memory_ticket = Some(ticket);
    let later = Instant::now() + Duration::from_secs(3600);
    fixture.owner.capture_memory_tool();
    fixture.owner.drive(later);
    let active = fixture.owner.active.as_ref().unwrap();
    assert!(active.tool.as_ref().unwrap().result.is_none());
    assert!(active.terminal.is_none());
    assert_ne!(active.failure_reason, Some("model_timeout"));
    // No tool timer is scheduled for the stalled read.
    assert!(
        fixture
            .owner
            .next_deadline(later)
            .is_none_or(|deadline| deadline > later)
    );
    fixture.owner.active.as_mut().unwrap().cancel = Some(journal::Cause::UserCancel);
    fixture.owner.capture_memory_tool();
    let active = fixture.owner.active.as_ref().unwrap();
    let tool = active.tool.as_ref().unwrap();
    let result = tool.result.as_ref().unwrap();
    assert_eq!(result.status, 6);
    assert!(result.text.is_empty());
    // The writer slot stays held until its job settles.
    assert!(!tool.memory_ticket.as_ref().unwrap().is_settled());
    controller.finish();
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
    fixture.set_tool("memory", &get_note("06"));
    let now = Instant::now();
    let (ticket, controller) = writer::test_support::controlled_ticket(false);
    let active = fixture.owner.active.as_mut().unwrap();
    let tool = active.tool.as_mut().unwrap();
    tool.intent_committed = true;
    tool.dispatched = true;
    tool.memory_ticket = Some(ticket);
    assert!(controller.publish(Ok(memory_reply())));
    fixture.owner.capture_memory_tool();
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
fn unsettled_memory_cancel_or_steering_never_delivers_late_body() {
    for steering in [false, true] {
        let mut fixture = Fixture::new();
        fixture.set_tool("memory", &get_note("06"));
        let now = Instant::now();
        let (ticket, controller) = writer::test_support::controlled_ticket(false);
        let active = fixture.owner.active.as_mut().unwrap();
        let tool = active.tool.as_mut().unwrap();
        tool.intent_committed = true;
        tool.dispatched = true;
        tool.memory_ticket = Some(ticket);
        active.cancel = Some(if steering {
            journal::Cause::Steering
        } else {
            journal::Cause::UserCancel
        });
        install_recorded_memory_cancel(&mut fixture);
        assert!(controller.publish(Ok(memory_reply())));
        fixture.owner.capture_memory_tool();
        fixture.owner.drive(now);
        let active = fixture.owner.active.as_ref().unwrap();
        let tool = active.tool.as_ref().unwrap();
        let result = tool.result.as_ref().unwrap();
        assert_eq!(result.status, 6);
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
            reserved_output_tokens: 2048,
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
    fixture.set_tool("service", r#"{"command":"tools"}"#);
    fixture
}
#[test]
fn inventory_requires_durable_intent_and_result_without_host_or_database_execution() {
    let mut fixture = inventory_fixture();
    fixture.active().tool.as_mut().unwrap().dispatched = true;
    fixture.owner.capture_inventory_tool();
    assert!(fixture.active().tool.as_ref().unwrap().result.is_none());
    fixture.acknowledge(Job::ToolIntent);
    fixture.owner.capture_inventory_tool();
    let active = fixture.owner.active.as_ref().unwrap();
    let tool = active.tool.as_ref().unwrap();
    assert_eq!(
        tool.record.tool_operation,
        Some(journal::ToolOperation::Tools)
    );
    assert_eq!(tool.record.decision, journal::ToolDecision::Run);
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
fn inventory_obeys_cancellation_and_disabled_tool_fences() {
    for status in [6, 2] {
        let mut fixture = inventory_fixture();
        fixture.acknowledge(Job::ToolIntent);
        let active = fixture.owner.active.as_mut().unwrap();
        let tool = active.tool.as_mut().unwrap();
        tool.dispatched = true;
        match status {
            6 => active.cancel = Some(journal::Cause::UserCancel),
            2 => active.tools_enabled = false,
            _ => unreachable!(),
        }
        fixture.owner.capture_inventory_tool();
        let active = fixture.owner.active.as_ref().unwrap();
        let result = active.tool.as_ref().unwrap().result.as_ref().unwrap();
        assert_eq!(result.status, status);
        assert!(result.text.is_empty());
        assert!(active.tool_cache.is_empty());
        assert!(active.tool_executor.is_settled());
    }
}

fn tool_record(name: &str, decision: journal::ToolDecision) -> journal::ToolRecord {
    journal::ToolRecord {
        name: name.into(),
        tool: journal::RegisteredTool::lookup(name),
        tool_operation: Some(journal::ToolOperation::Tools),
        decision,
        fault: None,
        busy: None,
        refusal: None,
        form: journal::ArgumentForm::Verbatim,
        arguments_bytes: 19,
        arguments: r#"{"command":"tools"}"#.into(),
        result_status: None,
        intent_frame: 0..1,
        result_frame: None,
        result_bytes: 0,
        preview: None,
        truncated: None,
        exit_code: None,
        create: None,
        exec_cmd: None,
    }
}
#[test]
fn committed_tool_activity_is_observable_before_model_text() {
    let mut fixture = inventory_fixture();
    let operation = fixture.active().turn.operation;
    let before = fixture.active().cursor;
    fixture.acknowledge(Job::ToolIntent);
    install_recorded_memory_cancel(&mut fixture);
    Arc::get_mut(fixture.owner.replay.as_mut().unwrap())
        .unwrap()
        .operations
        .get_mut(&operation)
        .unwrap()
        .tools
        .push(tool_record("service", journal::ToolDecision::Run));
    assert!(fixture.active().cursor > before);
    assert!(fixture.active().snapshot.is_empty());
    let reply = fixture
        .owner
        .observe(pb::Envelope::default(), operation, before)
        .unwrap();
    let Some(Body::ConversationEvent(event)) = reply.body else {
        panic!("activity event");
    };
    assert_eq!(event.kind, Some(1));
    assert!(event.text.is_none());
    assert_eq!(event.tools[0].name.as_deref(), Some("service"));
    assert_eq!(
        event.tools[0].arguments.as_deref(),
        Some(r#"{"command":"tools"}"#)
    );
    assert_eq!(event.tools[0].policy_decision, Some(1));
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

/// TCR-D13: a call that does not run projects its decision, fault, rendered
/// status and text; text that was not retained shows its size and reason.
#[test]
fn rejected_call_projection_uses_codes_and_never_guesses_text() {
    let mut fixture = inventory_fixture();
    let operation = fixture.active().turn.operation;
    install_recorded_memory_cancel(&mut fixture);
    let mut rejected = tool_record("project", journal::ToolDecision::InvalidArguments);
    rejected.tool_operation = Some(journal::ToolOperation::ReadFile);
    rejected.fault = Some(journal::ToolFault::MissingField);
    let mut oversized = tool_record("", journal::ToolDecision::InvalidArguments);
    oversized.tool_operation = None;
    oversized.fault = Some(journal::ToolFault::ArgumentsOverLimit);
    oversized.form = journal::ArgumentForm::NotRetained(journal::NotRetainedReason::Oversized);
    oversized.arguments = String::new();
    oversized.arguments_bytes = 70_000;
    oversized.name = "n".repeat(300);
    Arc::get_mut(fixture.owner.replay.as_mut().unwrap())
        .unwrap()
        .operations
        .get_mut(&operation)
        .unwrap()
        .tools
        .extend([rejected, oversized]);
    let reply = fixture
        .owner
        .observe(pb::Envelope::default(), operation, 0)
        .unwrap();
    let Some(Body::ConversationEvent(event)) = reply.body else {
        panic!("activity event");
    };
    let first = &event.tools[0];
    assert_eq!((first.state, first.status), (Some(3), Some(3)));
    assert_eq!((first.policy_decision, first.fault), (Some(2), Some(6)));
    assert_eq!(
        first.preview.as_deref(),
        Some("invalid_arguments: missing_field")
    );
    let second = &event.tools[1];
    assert_eq!(
        second.arguments.as_deref(),
        Some("[not retained: oversized, 70000 bytes]")
    );
    assert_eq!(
        second.name.as_ref().map(String::len),
        Some(64),
        "display prefix (TCR-D26)"
    );
    assert_eq!(second.fault, Some(12));
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

fn exec_cmd_token() -> String {
    ["ghp_", &"xV7".repeat(12)].concat()
}
fn exec_cmd_wire(ordinal: u32, command: &str) -> mp::ToolCall {
    wire(
        ordinal,
        "exec_cmd",
        &serde_json::json!({ "command": command, "cwd": "." }).to_string(),
    )
}

/// J1T-03 (owner layer): a rewrite over 8,192 bytes is a recorded refusal
/// decided at `ToolCall`; its kind 14 encodes within one frame, so the append
/// cannot fail on size and make the service unavailable.
#[test]
fn j1t_03_overlong_rewrite_is_a_recorded_refusal() {
    let mut fixture = Fixture::new();
    fresh_turn(&mut fixture);
    let unit = ["curl -u admin:", "Sup3r", "S3cret", " x;"].concat();
    let long = format!(
        "{}echo {}",
        unit.repeat(10),
        "f".repeat(8192 - 60 - unit.len() * 10 - 5 + 1)
    );
    assert!(long.len() <= 8192);
    propose(&mut fixture, exec_cmd_wire(1, &long));
    let record = pending_record(&mut fixture);
    assert_eq!(record.decision, journal::ToolDecision::Refused);
    assert_eq!(
        record.refusal.map(|r| r.code),
        Some(journal::RefusalCode::RewrittenCommandTooLong)
    );
    assert!(encodes(&record).len() <= 65_050);
    assert!(!record.arguments.contains("Sup3rS3cret"));
    assert!(fixture.owner.unavailable.is_none());
    fixture.acknowledge(Job::ToolIntent);
    assert_eq!(sent(&mut fixture)[0].status, Some(3));
}

/// J1T-04 (owner layer, memory inspection) and busy: with no payload store
/// the call is recorded `busy` with the rewrite. Neither the live call, the
/// record nor the cache holds the raw command; an equal resend matches and a
/// different value with the same rewrite is a protocol fault.
#[test]
fn j1t_04_live_tool_and_cache_hold_no_raw_command() {
    let mut fixture = Fixture::new();
    install_recorded_memory_cancel(&mut fixture);
    fresh_turn(&mut fixture);
    let token = exec_cmd_token();
    let raw = format!("printf '%s' \"{token}\" | wc -c");
    let first = exec_cmd_wire(1, &raw);
    propose(&mut fixture, first.clone());
    let record = pending_record(&mut fixture);
    assert_eq!(record.decision, journal::ToolDecision::Busy);
    assert_eq!(
        record.busy,
        Some(journal::ExecCmdBusyReason::PayloadUnavailable)
    );
    assert_eq!(
        record.number_kinds,
        vec![asura_platform::secrets::SecretKind::GithubToken]
    );
    assert!(record.arguments.contains("ASURA_SECRET_1"));
    let tool = fixture.active().tool.as_ref().unwrap();
    assert!(
        tool.call.is_none() && tool.exec_cmd.is_none(),
        "busy keeps no secret"
    );
    assert!(!format!("{:?}", tool.record).contains(&token));
    fixture.acknowledge(Job::ToolIntent);
    let active = fixture.active();
    assert_eq!(active.tool_cache.len(), 1);
    assert!(!format!("{:?}", active.tool_cache).contains(&token));
    assert_eq!(
        active.tool_cache[0].2.text.as_deref(),
        Some("exec_cmd_busy: payload_unavailable")
    );
    propose(&mut fixture, first);
    assert!(fixture.active().failure_reason.is_none());
    let other = ["ghp_", &"qQ2".repeat(12)].concat();
    propose(
        &mut fixture,
        exec_cmd_wire(1, &format!("printf '%s' \"{other}\" | wc -c")),
    );
    assert_eq!(
        fixture.active().failure_reason,
        Some("model_protocol_fault")
    );
}

/// J1T-05 unit (J1-Q3): no tool call has a time bound, so a result
/// committed long after it started never fails the turn.
#[test]
fn j1t_05_late_tool_result_never_fails_the_turn() {
    let mut fixture = Fixture::new();
    let now = Instant::now();
    fixture.stage_result();
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

/// A real private journal with an accepted, started turn; the fixture owner
/// appends through the real writer and acknowledges each real reply.
struct Journaled {
    fixture: Fixture,
    _serial: std::sync::MutexGuard<'static, ()>,
}
impl std::ops::Deref for Journaled {
    type Target = Fixture;
    fn deref(&self) -> &Fixture {
        &self.fixture
    }
}
impl std::ops::DerefMut for Journaled {
    fn deref_mut(&mut self) -> &mut Fixture {
        &mut self.fixture
    }
}
impl Drop for Journaled {
    fn drop(&mut self) {
        if let Some(writer) = &self.fixture.owner.writer {
            writer.close();
        }
        let end = Instant::now() + Duration::from_secs(10);
        while self
            .fixture
            .owner
            .writer
            .as_mut()
            .is_some_and(|w| !w.settled())
            && Instant::now() < end
        {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}
fn wait_reply(writer: &writer::WriterHandle, command: writer::Command) -> writer::Reply {
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
impl Journaled {
    fn new() -> Self {
        let serial = crate::TEST_WRITER.lock().unwrap_or_else(|p| p.into_inner());
        let mut fixture = Fixture::new();
        let writer = writer::WriterHandle::start(fixture.owner.runtime.clone()).unwrap();
        wait_reply(&writer, writer::Command::Initialize { request: [30; 16] });
        let project_path = fixture.path.join("project");
        std::fs::create_dir(&project_path).unwrap();
        let registered = wait_reply(
            &writer,
            writer::Command::Register {
                request: [31; 16],
                location: project_path.to_str().unwrap().into(),
            },
        )
        .replay
        .unwrap();
        let project = *registered.projects.keys().next().unwrap();
        let revision = registered.revision;
        drop(registered);
        let prepared = wait_reply(
            &writer,
            writer::Command::Prepare {
                project,
                conversation: None,
                expected_generation: 0,
                prompt: "observe".into(),
            },
        )
        .prepared
        .unwrap();
        let active = fixture.active();
        let mut turn = active.turn.clone();
        turn.project = project;
        turn.prompt = "observe".into();
        turn.configuration_digest = prepared.configuration_digest;
        turn.reserved_output_tokens = 2048;
        turn.digest = journal::request_digest(journal::Request::Submit {
            project,
            conversation: None,
            expected_generation: 0,
            prompt: "observe",
        })
        .unwrap();
        let state = wait_reply(
            &writer,
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
        let state = wait_reply(
            &writer,
            writer::Command::Append {
                expected_revision: state.revision,
                record: journal::Record::StartAuthorized(journal::StartAuthorized {
                    operation: turn.operation,
                    generation: 1,
                    helper: active.helper,
                    input_digest: turn.input_digest,
                }),
                admission: None,
            },
        )
        .replay
        .unwrap();
        active.turn = turn;
        fixture.owner.replay = Some(state);
        fixture.owner.writer = Some(writer);
        fresh_turn(&mut fixture);
        Self {
            fixture,
            _serial: serial,
        }
    }
    /// Waits for the submitted append and acknowledges its real reply.
    fn settle(&mut self) -> &'static str {
        let (mut ticket, job) = self.fixture.owner.pending.take().expect("an append");
        let end = Instant::now() + Duration::from_secs(10);
        let reply = loop {
            if let Some(reply) = ticket.poll() {
                break reply;
            }
            assert!(Instant::now() < end);
            std::thread::sleep(Duration::from_millis(1));
        };
        let tag = match &job {
            Job::Cancel => "cancel",
            Job::ToolIntent => "tool_call",
            Job::ToolResult => "tool_result",
            Job::Finish => "finish",
            _ => "other",
        };
        assert!(reply.is_ok(), "{tag}: {:?}", reply.as_ref().err());
        self.fixture.owner.job_done(job, reply, Instant::now());
        assert!(
            self.fixture.owner.unavailable.is_none(),
            "{:?}",
            self.fixture.owner.unavailable
        );
        tag
    }
    /// Every record of the journal, in order.
    fn records(&self) -> Vec<journal::Record> {
        let bytes =
            std::fs::read(self.fixture.path.join(".asura/state/control/slot-0.log")).unwrap();
        let mut out = Vec::new();
        let mut offset = 0;
        while offset < bytes.len() {
            let length =
                u32::from_be_bytes(bytes[offset + 12..offset + 16].try_into().unwrap()) as usize;
            let frame = &bytes[offset..offset + length];
            out.push(
                journal::decode_record(
                    u16::from_be_bytes([frame[10], frame[11]]),
                    &frame[144..frame.len() - 40],
                )
                .unwrap(),
            );
            offset += length;
        }
        out
    }
}

/// TCR-I07 (owner part, real journal): cancel arrives before the decision
/// commits. The journal holds CancelRequested, then kind 14 `cancelled`,
/// then the terminal, and no ToolResult is sent to the helper.
#[test]
fn tcr_i07_cancel_before_commit_records_cancelled_then_terminal() {
    let mut journaled = Journaled::new();
    propose(
        &mut journaled,
        wire(1, "service", r#"{"command":"status"}"#),
    );
    assert_eq!(
        journaled.active().tool.as_ref().unwrap().record.decision,
        journal::ToolDecision::Run
    );
    journaled.active().cancel = Some(journal::Cause::UserCancel);
    journaled.owner.drive(Instant::now());
    assert!(journaled.settle() == "cancel");
    journaled.owner.drive(Instant::now());
    assert!(journaled.settle() == "tool_call");
    assert!(journaled.active().tool.is_none());
    assert!(journaled.active().model.tool_test_results().is_empty());
    // The helper settles; the terminal follows.
    journaled.active().model = ModelOwner::settled_test_fixture();
    journaled.owner.drive(Instant::now());
    assert!(journaled.settle() == "finish");
    let tail: Vec<_> = journaled
        .records()
        .into_iter()
        .rev()
        .take(3)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    assert!(
        matches!(&tail[0], journal::Record::CancelRequested(_)),
        "{tail:?}"
    );
    match &tail[1] {
        journal::Record::ToolCall(call) => {
            assert_eq!(call.decision, journal::ToolDecision::Cancelled);
            assert_eq!(call.name, "service");
            assert_eq!(call.arguments, r#"{"command":"status"}"#);
            assert_eq!(call.tool_operation, Some(journal::ToolOperation::Status));
        }
        other => panic!("kind 14 expected, got {other:?}"),
    }
    match &tail[2] {
        journal::Record::TurnTerminal(terminal) => {
            assert_eq!(terminal.kind, journal::TerminalKind::Cancelled)
        }
        other => panic!("terminal expected, got {other:?}"),
    }
}

/// TCR-I01 (owner part, real journal): the rejected call is one kind 14 with
/// no kind 15; the corrected call runs and is followed by its kind 15.
#[test]
fn tcr_i01_real_journal_holds_rejected_record_without_result() {
    let mut journaled = Journaled::new();
    propose(
        &mut journaled,
        wire(1, "project", r#"{"command":"read_file"}"#),
    );
    journaled.owner.drive(Instant::now());
    assert!(journaled.settle() == "tool_call");
    assert_eq!(sent(&mut journaled)[0].status, Some(3));
    propose(&mut journaled, wire(2, "service", r#"{"command":"tools"}"#));
    journaled.owner.drive(Instant::now());
    assert!(journaled.settle() == "tool_call");
    journaled.owner.drive(Instant::now());
    journaled.owner.capture_inventory_tool();
    journaled.owner.drive(Instant::now());
    assert!(journaled.settle() == "tool_result");
    let records = journaled.records();
    let kinds: Vec<u16> = records.iter().map(journal::Record::kind).collect();
    let start = kinds.iter().position(|k| *k == 14).unwrap();
    assert_eq!(&kinds[start..], &[14, 14, 15]);
    let journal::Record::ToolCall(first) = &records[start] else {
        unreachable!()
    };
    assert_eq!(first.fault, Some(journal::ToolFault::MissingField));
    let replay = journaled.owner.replay.clone().unwrap();
    let op = &replay.operations[&journaled.owner.active.as_ref().unwrap().turn.operation];
    assert_eq!(op.tools.len(), 2);
    assert!(op.tools[0].result_frame.is_none());
    assert_eq!(op.tools[1].result_status, Some(1));
}

/// TCR-U13 (service part, real journal): replay recomputes the same exec_cmd
/// job and payload IDs that the live path derives for the same call.
#[test]
fn tcr_u13_replay_exec_cmd_ids_equal_live_derivation() {
    let mut journaled = Journaled::new();
    propose(&mut journaled, exec_cmd_wire(1, "printf ok"));
    // No payload store here: the decision is `busy`; the same record with
    // decision `run` is what an admitted call commits.
    let mut record = journaled.active().tool.as_ref().unwrap().record.clone();
    assert_eq!(record.decision, journal::ToolDecision::Busy);
    record.decision = journal::ToolDecision::Run;
    record.busy = None;
    let mut tool = journaled.active().tool.take().unwrap();
    tool.record = record;
    let call = tools::Call {
        operation: journaled.active().turn.operation,
        generation: 1,
        ordinal: 1,
        arguments: tools::Arguments::ExecCmd {
            command: "printf ok".into(),
            cwd: ".".into(),
        },
    };
    tool.call = Some(call.clone());
    journaled.active().tool = Some(tool);
    journaled.owner.drive(Instant::now());
    assert!(journaled.settle() == "tool_call");
    let replay = journaled.owner.replay.clone().unwrap();
    let live = crate::exec_cmd_worker::ExecCmdRun::derive(replay.installation_id, &call).unwrap();
    let derived = replay.operations[&call.operation].tools[0]
        .exec_cmd
        .clone()
        .unwrap();
    assert_eq!(
        (derived.job, derived.stdout_payload, derived.stderr_payload),
        (live.job, live.stdout_payload, live.stderr_payload)
    );
    // Restore an unlocked state for teardown: the call never ran.
    journaled.active().tool = None;
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
