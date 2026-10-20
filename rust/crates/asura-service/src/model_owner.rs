//! One model operation: bounded preparation, private protocol, and exact-child settlement.
use asura_control::model::{self, Direction, ModelCodec, pb};
use asura_platform::{ModelProcess, RuntimeDirectory};
use pb::envelope::Body;
use std::{
    collections::VecDeque,
    io::{self, Read, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
#[derive(Clone, Copy)]
pub(crate) struct PackageIdentity {
    pub build_id: [u8; 32],
    pub schema_digest: [u8; 32],
    pub helper_digest: [u8; 32],
    pub metallib_digest: Option<[u8; 32]>,
}
#[derive(Clone, Debug)]
pub(crate) struct Selection {
    pub model: String,
    pub asset_root: Option<String>,
    pub endpoint: Option<String>,
    pub model_capabilities: Option<u32>,
}
#[derive(Debug)]
pub(crate) enum ModelEvent {
    Inventory {
        models: Vec<pb::ModelInventoryEntry>,
        issues: Vec<pb::ModelInventoryIssue>,
    },
    Available {
        context_tokens: u32,
        reported_context_tokens: u32,
        context_source: u32,
        model_name: Option<String>,
        tools_available: bool,
        /// Bytes of the project tool definitions; the composer's overhead `S`.
        tool_schema_bytes: u32,
        capabilities: crate::model::CapabilityProfile,
    },
    InferenceUsage(pb::InferenceUsage),
    ContextMeasured {
        input_tokens: u32,
        capacity_tokens: u32,
    },
    Ready,
    /// One validated revision: keep `offset` bytes, append `text`
    /// (response-text-transfer.md). `thinking` revises the thinking text the
    /// same way (thinking-level.md).
    Revision {
        revision: u64,
        offset: u64,
        text: String,
        thinking: Option<(u64, String)>,
    },
    Terminal {
        outcome: i32,
        reason: i32,
        usage_tokens: Option<u64>,
    },
    ToolCall(pb::ToolCall),
    Failed(&'static str),
}
/// Kept wind-down (owner, 2026-10-16): cooperative cancel grace, then the
/// grace after SIGTERM before SIGKILL. Bounds Asura's own cleanup only.
const WIND_DOWN_GRACE: Duration = Duration::from_millis(250);
/// Kept wind-down: reap observation after each later escalation step.
const REAP_OBSERVATION: Duration = Duration::from_secs(1);
/// The helper wait a turn is in, for the waiting step display only
/// (owner, 2026-10-16). It never ends work and arms no timer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum HelperStep {
    /// Package copy, checks and spawn.
    Preparing,
    /// Spawned; waiting for the helper's Hello (provider load).
    Hello,
    /// Begin sent; input transfer until Ready.
    Input,
    /// Start sent; generation.
    Running,
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Preparing,
    Hello,
    Available,
    Input,
    Ready,
    Running,
    Draining,
    Settled,
}
pub(crate) struct ModelOwner {
    preparation: Option<JoinHandle<asura_platform::Result<ModelProcess>>>,
    cancel_preparation: Arc<AtomicBool>,
    child: Option<ModelProcess>,
    cleanup: Option<JoinHandle<()>>,
    cleanup_pending: Option<Arc<Mutex<Option<ModelProcess>>>>,
    cleanup_retry: Option<Instant>,
    identity: PackageIdentity,
    selection: Selection,
    tools_available: bool,
    inventory_only: bool,
    phase: Phase,
    /// Wind-down only (owner, 2026-10-16): set when draining after cancel or
    /// a terminal to escalate 250 ms -> SIGTERM -> 250 ms -> SIGKILL -> 1 s
    /// reap observation. Preparation, Hello, input transfer and the
    /// Ready-to-Start handoff have no time limit: each step ends on the next
    /// message, helper exit, a protocol fault, cancellation or shutdown.
    deadline: Option<Instant>,
    drain_step: u8,
    operation: [u8; 16],
    generation: u64,
    codec: ModelCodec,
    outgoing: VecDeque<Vec<u8>>,
    written: usize,
    incoming: Vec<u8>,
    input: Option<Vec<u8>>,
    /// Sliding input credit (context-composer.md, streamed input transfer).
    input_sent: u64,
    input_accepted: u64,
    input_granted: u64,
    /// The current output transfer's bytes: at most one revision.
    snapshot: Vec<u8>,
    revision: u64,
    chunks: u64,
    total_chunks: u64,
    total_bytes: u64,
    /// The conversation owner withholds the next revision's credit while
    /// its journal backlog is large (response-text-transfer.md).
    output_paused: bool,
    /// The transfer whose credit is granted; the next is owed while paused.
    credited: u64,
    terminal_seen: bool,
    faulted: bool,
    tools_enabled: bool,
    pending_tool: Option<u32>,
    measured_context: bool,
    usage_reports: u32,
    effective_context: Option<u32>,
    /// Hello field 23; Begin sends only a request within it.
    thinking_controls: u32,
    completion: crate::completion::Completion,
    cleanup_completion: crate::completion::Completion,
    wake: Option<asura_platform::events::WakeSender>,
}
impl ModelOwner {
    pub(crate) fn register(&mut self, wake: asura_platform::events::WakeSender) {
        self.completion.register(wake.clone());
        self.cleanup_completion.register(wake.clone());
        self.wake = Some(wake);
    }
    pub(crate) fn interests(&self) -> Vec<asura_platform::PollInterest> {
        use std::os::fd::AsRawFd;
        let Some(child) = &self.child else {
            return Vec::new();
        };
        if self.phase == Phase::Draining {
            return Vec::new();
        }
        let mut interests = vec![asura_platform::PollInterest {
            fd: child.channel.as_raw_fd(),
            read: true,
            write: !self.outgoing.is_empty(),
        }];
        if let Some(fd) = child.diagnostics_fd() {
            interests.push(asura_platform::PollInterest {
                fd,
                read: true,
                write: false,
            });
        }
        interests
    }
    fn buffered_work(&self) -> bool {
        !self.incoming.is_empty()
    }
    pub(crate) fn next_deadline(&self, now: Instant) -> Option<Instant> {
        if self.phase == Phase::Settled {
            return None;
        }
        if self.buffered_work() {
            return Some(now);
        }
        let settlement = self
            .preparation
            .as_ref()
            .and_then(|_| self.completion.settlement_deadline(now));
        let cleanup = self
            .cleanup
            .as_ref()
            .and_then(|_| self.cleanup_completion.settlement_deadline(now));
        self.deadline
            .filter(|_| self.phase == Phase::Draining && self.child.is_some())
            .into_iter()
            .chain(settlement)
            .chain(cleanup)
            .chain(self.cleanup_retry)
            .min()
    }
    pub(crate) fn new(
        runtime: RuntimeDirectory,
        identity: PackageIdentity,
        selection: Selection,
    ) -> io::Result<Self> {
        Self::create(runtime, identity, selection, false)
    }
    pub(crate) fn inventory(
        runtime: RuntimeDirectory,
        identity: PackageIdentity,
        selection: Selection,
    ) -> io::Result<Self> {
        Self::create(runtime, identity, selection, true)
    }
    fn create(
        runtime: RuntimeDirectory,
        identity: PackageIdentity,
        selection: Selection,
        inventory_only: bool,
    ) -> io::Result<Self> {
        let resource_digest = if selection.model.starts_with("mlx:") {
            Some(identity.metallib_digest.ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "MLX resource identity unavailable")
            })?)
        } else {
            None
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        let token = cancelled.clone();
        let completion = crate::completion::Completion::default();
        let signal = completion.clone();
        let task = thread::Builder::new()
            .name("asura-model-prepare".into())
            .spawn(move || {
                let _completion = signal.guard();
                let executable = std::env::current_exe()?.canonicalize()?;
                let helper = executable
                    .parent()
                    .ok_or(asura_platform::Error::Unavailable)?
                    .join("asura-model");
                ModelProcess::spawn_with_resource(
                    &runtime,
                    &helper,
                    identity.helper_digest,
                    resource_digest,
                    &token,
                )
            })?;
        Ok(Self {
            preparation: Some(task),
            cancel_preparation: cancelled,
            child: None,
            cleanup: None,
            cleanup_pending: None,
            cleanup_retry: None,
            identity,
            selection,
            tools_available: false,
            inventory_only,
            phase: Phase::Preparing,
            deadline: None,
            drain_step: 0,
            operation: [0; 16],
            generation: 0,
            codec: ModelCodec::new(),
            outgoing: VecDeque::new(),
            written: 0,
            incoming: Vec::new(),
            input: None,
            input_sent: 0,
            input_accepted: 0,
            input_granted: 0,
            snapshot: Vec::new(),
            revision: 0,
            chunks: 0,
            total_chunks: 0,
            total_bytes: 0,
            output_paused: false,
            credited: 0,
            terminal_seen: false,
            faulted: false,
            tools_enabled: false,
            pending_tool: None,
            measured_context: false,
            usage_reports: 0,
            effective_context: None,
            thinking_controls: 0,
            completion,
            cleanup_completion: Default::default(),
            wake: None,
        })
    }
    pub(crate) fn begin(
        &mut self,
        operation: [u8; 16],
        generation: u64,
        input: Vec<u8>,
        enable_tools: bool,
        thinking: asura_control::thinking::Request,
    ) -> Result<(), &'static str> {
        if self.phase != Phase::Available || generation == 0 || operation == [0; 16] {
            return Err("model_not_ready");
        }
        // Only a form the helper reported (thinking-level.md).
        if thinking.control() & !self.thinking_controls != 0 {
            return Err("model_input_invalid");
        }
        model::decode_input(&input).map_err(|_| "model_input_invalid")?;
        // A protocol sanity bound on allocation, not a content limit.
        if (input.len() as u64) > u64::from(self.effective_context.unwrap_or(0)).saturating_mul(8) {
            return Err("model_input_invalid");
        }
        // Absence of native tool support is negotiated before inference. A real
        // provider failure never retries the turn without tools.
        let enable_tools = enable_tools && self.tools_available;
        self.tools_enabled = enable_tools;
        self.operation = operation;
        self.generation = generation;
        self.queue(Body::Begin(pb::Begin {
            model: Some(self.selection.model.clone()),
            input_bytes: Some(input.len() as u64),
            // No output cap: the model writes until it stops or its window is
            // full (owner, 2026-10-19; turn-usage-helper.md, request admission).
            enable_project_tools: enable_tools.then_some(true),
            thinking: (thinking != asura_control::thinking::Request::None)
                .then_some(thinking as i32),
        }))?;
        self.input = Some(input);
        self.input_sent = 0;
        self.input_accepted = 0;
        self.input_granted = 0;
        self.phase = Phase::Input;
        Ok(())
    }
    pub(crate) fn tool_result(&mut self, result: pb::ToolResult) -> Result<(), &'static str> {
        if self.phase != Phase::Running
            || !self.tools_enabled
            || self.pending_tool != result.ordinal
        {
            return Err("model_not_ready");
        }
        self.queue(Body::ToolResult(result))?;
        self.pending_tool = None;
        Ok(())
    }
    pub(crate) fn start(&mut self) -> Result<(), &'static str> {
        if self.phase != Phase::Ready {
            return Err("model_not_ready");
        }
        self.queue(Body::Start(pb::Start {}))?;
        self.credit(2)?;
        self.credited = 2;
        self.phase = Phase::Running;
        Ok(())
    }
    /// Withhold or release the next revision's credit; called on every
    /// conversation pass, so a credit refused by a full queue is retried. A
    /// transfer already credited completes; nothing waits on a timer.
    pub(crate) fn set_output_paused(&mut self, paused: bool) {
        self.output_paused = paused;
        self.release_credit();
    }
    fn release_credit(&mut self) {
        let next = self.revision + 2;
        if !self.output_paused
            && self.phase == Phase::Running
            && self.credited < next
            && self.credit(next).is_ok()
        {
            self.credited = next;
        }
    }
    pub(crate) fn cancel(&mut self, reason: i32, now: Instant) {
        if self.phase == Phase::Settled || self.phase == Phase::Draining {
            return;
        }
        tracing::info!(stage = "cancel", reason, phase = ?self.phase, "model_helper_diagnostic");
        self.cancel_preparation.store(true, Ordering::Release);
        let partial = self.written != 0;
        self.outgoing.clear();
        self.written = 0;
        if partial && let Some(child) = &self.child {
            let _ = child.channel.shutdown(std::net::Shutdown::Both);
        }
        if !partial && self.generation > 0 {
            let _ = self.queue(Body::Cancel(pb::Cancel {
                reason: Some(reason),
            }));
        }
        self.phase = Phase::Draining;
        self.deadline = Some(now + WIND_DOWN_GRACE);
        self.drain_step = 0;
    }
    /// The helper wait this owner is in; `None` outside the four waits.
    pub(crate) fn helper_step(&self) -> Option<HelperStep> {
        match self.phase {
            Phase::Preparing => Some(HelperStep::Preparing),
            Phase::Hello => Some(HelperStep::Hello),
            Phase::Input => Some(HelperStep::Input),
            Phase::Running => Some(HelperStep::Running),
            Phase::Available | Phase::Ready | Phase::Draining | Phase::Settled => None,
        }
    }
    pub(crate) fn is_settled(&self) -> bool {
        self.phase == Phase::Settled
            && self.preparation.is_none()
            && self.child.is_none()
            && self.cleanup.is_none()
            && self.cleanup_pending.is_none()
    }
    fn queue(&mut self, body: Body) -> Result<(), &'static str> {
        // Keep one control slot available even when input or credits are queued.
        let limit = if matches!(body, Body::Cancel(_)) {
            8
        } else {
            7
        };
        if self.outgoing.len() >= limit {
            return Err("model_queue_full");
        }
        let hello = matches!(body, Body::Hello(_));
        let envelope = pb::Envelope {
            operation_id: (!hello).then(|| self.operation.to_vec()),
            generation: (!hello).then_some(self.generation),
            body: Some(body),
        };
        self.outgoing.push_back(
            model::encode_frame(&envelope, Direction::ServiceToHelper)
                .map_err(|_| "model_protocol_fault")?,
        );
        Ok(())
    }
    fn credit(&mut self, transfer: u64) -> Result<(), &'static str> {
        self.queue(Body::Credit(pb::Credit {
            transfer_id: Some(transfer),
            direction: Some(2),
            accepted_bytes: Some(0),
            granted_bytes: Some(65536),
        }))
    }
    fn fail(&mut self, reason: &'static str, now: Instant, events: &mut Vec<ModelEvent>) {
        if self.phase != Phase::Draining && self.phase != Phase::Settled {
            tracing::warn!(stage = "channel", class = reason, phase = ?self.phase, "model_helper_diagnostic");
            self.faulted = true;
            events.push(ModelEvent::Failed(reason));
            self.cancel(4, now);
        }
    }
    pub(crate) fn poll(&mut self, now: Instant) -> Vec<ModelEvent> {
        let mut events = Vec::new();
        if self
            .preparation
            .as_ref()
            .is_some_and(JoinHandle::is_finished)
        {
            match self.preparation.take().unwrap().join() {
                Ok(Ok(child)) => {
                    self.child = Some(child);
                    if self.phase == Phase::Preparing {
                        self.phase = Phase::Hello;
                        let hello = Body::Hello(pb::Hello {
                            local_tool_destination: None,
                            thinking_controls: None,
                            supported_capabilities: None,
                            capability_source: None,
                            model_capabilities: self.selection.model_capabilities,
                            selected_model: Some(self.selection.model.clone()),
                            asset_root: self.selection.asset_root.clone(),
                            endpoint: self.selection.endpoint.clone(),
                            build_id: Some(self.identity.build_id.to_vec()),
                            schema_digest: Some(self.identity.schema_digest.to_vec()),
                            max_frame_bytes: Some(65536),
                            tool_schema_bytes: None,
                            availability: Some(3),
                            context_tokens: None,
                            reported_context_tokens: None,
                            context_source: None,
                            model_name: None,
                            reason: Some(0),
                            inventory_only: self.inventory_only.then_some(true),
                            models: Vec::new(),
                            issues: Vec::new(),
                        });
                        if self.queue(hello).is_err() {
                            self.fail("model_protocol_fault", now, &mut events);
                        }
                    }
                }
                failure => {
                    let (class, code) = match failure {
                        Ok(Err(asura_platform::Error::Io(error))) => {
                            ("preparation_io", error.raw_os_error().unwrap_or(0))
                        }
                        Ok(Err(asura_platform::Error::UnsafeRuntime)) => ("preparation_unsafe", 0),
                        // Preparation reports cancellation as `Deadline`.
                        Ok(Err(asura_platform::Error::Deadline)) => ("preparation_cancelled", 0),
                        Ok(Err(asura_platform::Error::Absent)) => ("preparation_absent", 0),
                        Ok(Err(_)) => ("preparation_unavailable", 0),
                        Err(_) => ("preparation_panic", 0),
                        Ok(Ok(_)) => unreachable!("successful preparation handled above"),
                    };
                    tracing::warn!(
                        stage = "preparation",
                        class,
                        code,
                        "model_helper_diagnostic"
                    );
                    if self.phase != Phase::Draining {
                        events.push(ModelEvent::Failed("model_helper_unavailable"));
                    }
                    self.phase = Phase::Settled;
                }
            }
        }
        if self.child.is_some() {
            if let Err(reason) = self.io_pass(&mut events) {
                if self.terminal_seen
                    && reason.contains("protocol")
                    && self.phase == Phase::Draining
                {
                    self.terminal_seen = false;
                    self.faulted = true;
                    events.push(ModelEvent::Failed(reason));
                } else {
                    self.fail(reason, now, &mut events);
                }
            }
            if self.terminal_seen && self.phase != Phase::Draining {
                self.phase = Phase::Draining;
                self.deadline = Some(now + WIND_DOWN_GRACE);
                self.drain_step = 0;
            }
            if let Some(child) = self.child.as_mut() {
                let _ = child.poll_diagnostics();
                for (class, code) in child.take_failure_diagnostics() {
                    tracing::warn!(class, code, "model_helper_diagnostic");
                }
                if self.phase == Phase::Draining && child.try_reap().unwrap_or(false) {
                    let _ = child.poll_diagnostics();
                    for (class, code) in child.take_failure_diagnostics() {
                        tracing::warn!(stage = "exit", class, code, "model_helper_diagnostic");
                    }
                    self.incoming.clear();
                    self.cleanup_pending = Some(Arc::new(Mutex::new(self.child.take())));
                } else if self.phase == Phase::Draining
                    && self.deadline.is_some_and(|deadline| now >= deadline)
                {
                    let _ = child.terminate(self.drain_step > 0);
                    self.drain_step = self.drain_step.saturating_add(1);
                    self.deadline = Some(
                        now + if self.drain_step == 1 {
                            WIND_DOWN_GRACE
                        } else {
                            REAP_OBSERVATION
                        },
                    );
                }
            }
        } else if self.phase == Phase::Draining
            && self.preparation.is_none()
            && self.cleanup_pending.is_none()
        {
            self.phase = Phase::Settled;
        }
        if self.cleanup.is_none()
            && self.cleanup_retry.is_none_or(|deadline| now >= deadline)
            && let Some(pending) = &self.cleanup_pending
        {
            let pending = pending.clone();
            let completion = self.cleanup_completion.clone();
            self.cleanup = thread::Builder::new()
                .name("asura-model-cleanup".into())
                .spawn(move || {
                    let _completion = completion.guard();
                    let child = pending.lock().expect("exclusive cleanup slot").take();
                    drop(child);
                })
                .ok();
            self.cleanup_retry = self
                .cleanup
                .is_none()
                .then_some(now + Duration::from_millis(100));
        }
        if self.cleanup.as_ref().is_some_and(JoinHandle::is_finished) {
            let _ = self.cleanup.take().unwrap().join();
            self.cleanup_pending = None;
            self.phase = Phase::Settled;
        }
        events
    }
    fn io_pass(&mut self, events: &mut Vec<ModelEvent>) -> Result<(), &'static str> {
        let child = self.child.as_mut().ok_or("model_helper_failed")?;
        let mut budget = 65536;
        while let Some(frame) = self.outgoing.front() {
            if budget == 0 {
                break;
            }
            let end = frame.len().min(self.written + budget);
            match child.channel.write(&frame[self.written..end]) {
                Ok(0) => return Err("model_helper_failed"),
                Ok(n) => {
                    self.written += n;
                    budget -= n;
                    if self.written == frame.len() {
                        self.outgoing.pop_front();
                        self.written = 0;
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    break;
                }
                Err(_) => {
                    // A helper can close after writing its terminal while our next credit
                    // is queued. Drain its buffered terminal before deciding it failed.
                    self.outgoing.clear();
                    self.written = 0;
                    break;
                }
            }
        }
        // Retain at most one OS read in addition to the bounded frame decoder.
        if self.incoming.is_empty() {
            let mut bytes = [0u8; 16384];
            match child.channel.read(&mut bytes) {
                Ok(0) => {
                    if !self.terminal_seen {
                        return Err("model_helper_failed");
                    }
                }
                Ok(n) => self.incoming.extend_from_slice(&bytes[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(_) => return Err("model_helper_failed"),
            }
        }
        self.consume_input(events)
    }
    fn consume_input(&mut self, events: &mut Vec<ModelEvent>) -> Result<(), &'static str> {
        let mut frames = 0;
        while !self.incoming.is_empty() && frames < 8 && events.len() < 8 {
            let used = self
                .codec
                .push(&self.incoming)
                .map_err(|_| "model_protocol_fault")?;
            self.incoming.drain(..used);
            if let Some(envelope) = self
                .codec
                .next_frame(Direction::HelperToService)
                .map_err(|_| "model_protocol_fault")?
            {
                frames += 1;
                self.receive(envelope, events)?;
            } else {
                break;
            }
        }
        Ok(())
    }
    fn receive(
        &mut self,
        envelope: pb::Envelope,
        events: &mut Vec<ModelEvent>,
    ) -> Result<(), &'static str> {
        if self.faulted {
            return Ok(());
        }
        if self.terminal_seen {
            return Err("model_protocol_fault");
        }
        if self.phase == Phase::Hello {
            let Some(Body::Hello(hello)) = envelope.body else {
                return Err("model_protocol_fault");
            };
            if hello.build_id.as_deref() != Some(&self.identity.build_id)
                || hello.schema_digest.as_deref() != Some(&self.identity.schema_digest)
            {
                return Err("model_protocol_identity_mismatch");
            }
            if (hello.inventory_only == Some(true)) != self.inventory_only {
                return Err("model_protocol_fault");
            }
            if self.inventory_only {
                if hello.selected_model.as_deref() != Some("system") {
                    return Err("model_protocol_identity_mismatch");
                }
                events.push(ModelEvent::Inventory {
                    models: hello.models,
                    issues: hello.issues,
                });
                self.terminal_seen = true;
                self.cancel(3, Instant::now());
                return Ok(());
            }
            if hello.availability != Some(1) {
                events.push(ModelEvent::Failed("model_unavailable"));
                self.cancel(4, Instant::now());
                return Ok(());
            }
            // Validation requires the selector; there is no default.
            if hello.selected_model.as_deref() != Some(self.selection.model.as_str()) {
                return Err("model_protocol_identity_mismatch");
            }
            // The canonical codec validates the source against the selector,
            // including case-insensitive provider prefixes. Identity stays exact.
            self.effective_context = hello.context_tokens;
            self.thinking_controls = hello.thinking_controls.unwrap_or(0);
            // Tool support comes from the capability mask alone (owner, 2026-10-17).
            self.tools_available = hello
                .supported_capabilities
                .is_some_and(|mask| mask & 1 != 0)
                && matches!(hello.capability_source, Some(1..=3))
                && local_tool_destination(
                    &self.selection,
                    hello.local_tool_destination == Some(true),
                );
            self.phase = Phase::Available;
            events.push(ModelEvent::Available {
                context_tokens: hello.context_tokens.unwrap(),
                reported_context_tokens: hello.reported_context_tokens.unwrap(),
                context_source: hello.context_source.unwrap(),
                model_name: hello.model_name,
                tools_available: self.tools_available,
                tool_schema_bytes: hello.tool_schema_bytes.unwrap_or(0),
                capabilities: crate::model::CapabilityProfile {
                    supported: hello.supported_capabilities,
                    source: hello.capability_source,
                    thinking_controls: hello.thinking_controls.unwrap_or(0),
                },
            });
            return Ok(());
        }
        model::validate_identity(&envelope, &self.operation, self.generation)
            .map_err(|_| "model_protocol_identity_mismatch")?;
        match envelope.body.ok_or("model_protocol_fault")? {
            Body::Credit(credit) if self.phase == Phase::Input => {
                // The helper advances its window for every accepted chunk, so
                // credits for chunks already sent can follow InputEnd. They
                // only update the ledger (context-composer.md, CC-U16).
                let total = self
                    .input
                    .as_ref()
                    .map_or(self.input_sent, |input| input.len() as u64);
                let (Some(accepted), Some(granted)) = (credit.accepted_bytes, credit.granted_bytes)
                else {
                    return Err("model_protocol_fault");
                };
                if credit.transfer_id != Some(1)
                    || accepted < self.input_accepted
                    || accepted > self.input_sent
                    || granted <= self.input_granted
                    || granted < self.input_sent
                    || granted - accepted > model::CREDIT_WINDOW
                {
                    return Err("model_protocol_fault");
                }
                self.input_accepted = accepted;
                self.input_granted = granted;
                let chunk = model::MAX_CHUNK_BYTES as u64;
                // Send whole chunks only, so each ordinal names a fixed offset.
                loop {
                    let start = self.input_sent;
                    let end = (start + chunk).min(total);
                    if self.input.is_none() || start >= total || end > granted {
                        break;
                    }
                    let data =
                        self.input.as_ref().expect("input")[start as usize..end as usize].to_vec();
                    self.queue(Body::Chunk(pb::Chunk {
                        transfer_id: Some(1),
                        direction: Some(1),
                        ordinal: Some(start / chunk),
                        data: Some(data),
                        revision: Some(0),
                    }))?;
                    self.input_sent = end;
                }
                if self.input.is_some() && self.input_sent == total {
                    self.queue(Body::InputEnd(pb::InputEnd {
                        count: Some(total.div_ceil(chunk)),
                        total_bytes: Some(total),
                    }))?;
                    self.input = None;
                }
            }
            Body::Ready(_) if self.phase == Phase::Input && self.input.is_none() => {
                self.phase = Phase::Ready;
                events.push(ModelEvent::Ready);
            }
            Body::ToolCall(call) if self.phase == Phase::Running && self.tools_enabled => {
                if self.pending_tool.is_some() && self.pending_tool != call.ordinal {
                    return Err("model_protocol_fault");
                }
                self.pending_tool = call.ordinal;
                events.push(ModelEvent::ToolCall(call));
            }
            Body::InferenceUsage(value)
                if self.phase == Phase::Running || self.phase == Phase::Draining =>
            {
                // Consecutive ordinals with no per-turn request count (owner,
                // 2026-10-18); exhausting u32 is a known-size protocol fault.
                let Some(next) = self.usage_reports.checked_add(1) else {
                    return Err("model_protocol_fault");
                };
                if value.request_ordinal != Some(next) {
                    return Err("model_protocol_fault");
                }
                self.usage_reports = next;
                events.push(ModelEvent::InferenceUsage(value));
            }
            Body::ContextMeasured(value)
                if self.phase == Phase::Running
                    && !self.measured_context
                    && value.capacity_tokens == self.effective_context =>
            {
                self.measured_context = true;
                events.push(ModelEvent::ContextMeasured {
                    input_tokens: value.input_tokens.ok_or("model_protocol_fault")?,
                    capacity_tokens: value.capacity_tokens.ok_or("model_protocol_fault")?,
                });
            }
            Body::Chunk(chunk) if self.phase == Phase::Running || self.phase == Phase::Draining => {
                // Data only within credit, at most one revision per transfer.
                // No revision or cumulative output total (owner, 2026-10-18).
                if chunk.transfer_id != Some(self.revision + 2)
                    || self.credited != self.revision + 2
                    || chunk.revision != Some(self.revision + 1)
                    || chunk.ordinal != Some(self.chunks)
                {
                    return Err("model_protocol_fault");
                }
                let data = chunk.data.unwrap();
                if (self.snapshot.len() + data.len()) as u64 > model::MAX_REVISION_BYTES {
                    return Err("model_protocol_fault");
                }
                self.snapshot.extend_from_slice(&data);
                self.chunks += 1;
                self.total_chunks += 1;
                self.total_bytes += data.len() as u64;
            }
            Body::SnapshotEnd(end)
                if self.phase == Phase::Running || self.phase == Phase::Draining =>
            {
                if end.revision != Some(self.revision + 1)
                    || end.count != Some(self.chunks)
                    || end.total_bytes != Some(self.snapshot.len() as u64)
                {
                    return Err("model_protocol_fault");
                }
                self.revision += 1;
                self.chunks = 0;
                // The first `thinking_bytes` bytes revise the thinking text
                // (thinking-level.md). Each kept prefix ends on a character
                // boundary, so each appended part is UTF-8 by itself.
                let mut bytes = std::mem::take(&mut self.snapshot);
                let thinking = match (end.thinking_offset, end.thinking_bytes) {
                    (Some(offset), Some(count)) => {
                        let count = usize::try_from(count)
                            .ok()
                            .filter(|count| *count <= bytes.len())
                            .ok_or("model_protocol_fault")?;
                        let rest = bytes.split_off(count);
                        let thinking = std::mem::replace(&mut bytes, rest);
                        Some((
                            offset,
                            String::from_utf8(thinking).map_err(|_| "model_protocol_fault")?,
                        ))
                    }
                    (None, None) => None,
                    _ => return Err("model_protocol_fault"),
                };
                let text = String::from_utf8(bytes).map_err(|_| "model_protocol_fault")?;
                if self.phase == Phase::Running {
                    // The conversation owner releases the next credit after
                    // it applies this revision (`set_output_paused`).
                    events.push(ModelEvent::Revision {
                        revision: self.revision,
                        offset: end.offset.ok_or("model_protocol_fault")?,
                        text,
                        thinking,
                    });
                }
            }
            Body::Terminal(terminal)
                if matches!(
                    self.phase,
                    Phase::Running | Phase::Ready | Phase::Input | Phase::Draining
                ) =>
            {
                tracing::debug!(
                    stage = "terminal",
                    class = "terminal_received",
                    outcome = ?terminal.outcome,
                    reason = ?terminal.reason,
                    revision = ?terminal.last_revision,
                    chunks = ?terminal.count,
                    bytes = ?terminal.total_bytes,
                    "model_helper_diagnostic"
                );
                // A successful response cannot precede the service's durable tool result.
                // Failure/cancellation may end a suspended callback; its host worker still settles.
                if terminal.outcome == Some(1) && self.pending_tool.is_some() {
                    return Err("model_protocol_fault");
                }
                let expected_revision = self.revision + u64::from(self.chunks > 0);
                if terminal.count != Some(self.total_chunks)
                    || terminal.total_bytes != Some(self.total_bytes)
                    || !terminal.last_revision.is_some_and(|v| {
                        v == expected_revision
                            || terminal.outcome != Some(1)
                                && self.chunks == 0
                                && v == self.revision + 1
                    })
                    || terminal.outcome == Some(1)
                        && (!self.snapshot.is_empty() || self.revision == 0)
                {
                    tracing::warn!(
                        stage = "terminal",
                        class = "terminal_accounting",
                        received_revision = ?terminal.last_revision,
                        expected_revision,
                        received_chunks = ?terminal.count,
                        expected_chunks = self.total_chunks,
                        received_bytes = ?terminal.total_bytes,
                        expected_bytes = self.total_bytes,
                        partial_bytes = self.snapshot.len(),
                        "model_helper_diagnostic"
                    );
                    return Err("model_protocol_fault");
                }
                self.terminal_seen = true;
                events.push(ModelEvent::Terminal {
                    outcome: terminal.outcome.unwrap(),
                    reason: terminal.reason.unwrap(),
                    usage_tokens: terminal.usage_tokens,
                });
            }
            _ => return Err("model_protocol_fault"),
        }
        Ok(())
    }
}

fn local_tool_destination(selection: &Selection, attested: bool) -> bool {
    let Ok(identifier) = selection.model.parse::<crate::model::ModelIdentifier>() else {
        return false;
    };
    let Ok(resolved) = crate::model::resolve(identifier, &[crate::model::Capability::Text]) else {
        return false;
    };
    if resolved.provider().destination_kind() == crate::model::DestinationKind::OnDevice {
        return true;
    }
    if resolved.provider().name() != "ollama" || !attested {
        return false;
    }
    asura_storage::config::loopback_endpoint(
        selection
            .endpoint
            .as_deref()
            .unwrap_or(DEFAULT_OLLAMA_ENDPOINT),
    )
}

const DEFAULT_OLLAMA_ENDPOINT: &str = "http://127.0.0.1:11434";

/// Remote endpoints stay restricted to loopback until the execution threat
/// model is reviewed. Only Ollama uses a configured endpoint today.
pub(crate) fn endpoint_permitted(provider: &str, endpoint: Option<&str>) -> bool {
    provider != "ollama"
        || asura_storage::config::loopback_endpoint(endpoint.unwrap_or(DEFAULT_OLLAMA_ENDPOINT))
}

#[cfg(test)]
impl ModelOwner {
    /// An owner held in the helper wait for `step`, or with no step.
    pub(crate) fn step_test_fixture(step: Option<HelperStep>) -> Self {
        tests::owner(match step {
            Some(HelperStep::Preparing) => Phase::Preparing,
            Some(HelperStep::Hello) => Phase::Hello,
            Some(HelperStep::Input) => Phase::Input,
            Some(HelperStep::Running) => Phase::Running,
            None => Phase::Ready,
        })
    }
    pub(crate) fn tool_test_fixture() -> Self {
        let mut value = tests::owner(Phase::Running);
        value.tools_enabled = true;
        value.pending_tool = Some(1);
        value
    }
    /// An owner whose helper has settled (no child, no cleanup).
    pub(crate) fn settled_test_fixture() -> Self {
        tests::owner(Phase::Settled)
    }
    /// As if `revision` revisions had been received; the next is owed credit.
    pub(crate) fn response_test_revision(&mut self, revision: u64) {
        self.revision = revision;
    }
    /// Output transfer credits queued for the helper; drains the queue.
    pub(crate) fn response_test_credits(&mut self) -> Vec<u64> {
        tests::output_credits(self)
    }
    /// As if the helper had just sent the `ToolCall` at `ordinal`.
    pub(crate) fn tool_test_pending(&mut self, ordinal: u32) {
        self.pending_tool = Some(ordinal);
    }
    /// Every ToolResult queued for the helper, in order.
    pub(crate) fn tool_test_results(&self) -> Vec<pb::ToolResult> {
        self.outgoing
            .iter()
            .filter_map(|frame| {
                match model::decode_body(&frame[4..], Direction::ServiceToHelper)
                    .ok()?
                    .body
                {
                    Some(Body::ToolResult(result)) => Some(result),
                    _ => None,
                }
            })
            .collect()
    }
    /// Takes every queued ToolResult, as the transport would write them.
    pub(crate) fn tool_test_drain_results(&mut self) -> Vec<pb::ToolResult> {
        let results = self.tool_test_results();
        self.outgoing.clear();
        results
    }
    pub(crate) fn tool_test_has_delivery(&self, result: pb::ToolResult) -> bool {
        let frame = model::encode_frame(
            &pb::Envelope {
                operation_id: Some(self.operation.to_vec()),
                generation: Some(self.generation),
                body: Some(Body::ToolResult(result)),
            },
            Direction::ServiceToHelper,
        )
        .unwrap();
        self.outgoing.contains(&frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn owner_with_capacity(tokens: u32) -> ModelOwner {
        let mut owner = owner(Phase::Available);
        owner.effective_context = Some(tokens);
        owner
    }
    impl ModelOwner {
        /// Whether this credit would be refused, without changing the ledger.
        fn clone_ledger_check(&mut self, credit: pb::Envelope) -> bool {
            let saved = (
                self.input.clone(),
                self.input_sent,
                self.input_accepted,
                self.input_granted,
                self.outgoing.len(),
            );
            let refused = self.receive(credit, &mut Vec::new()).is_err();
            self.input = saved.0;
            self.input_sent = saved.1;
            self.input_accepted = saved.2;
            self.input_granted = saved.3;
            self.outgoing.truncate(saved.4);
            self.faulted = false;
            refused
        }
    }
    pub(super) fn owner(phase: Phase) -> ModelOwner {
        ModelOwner {
            preparation: None,
            cancel_preparation: Arc::new(AtomicBool::new(false)),
            child: None,
            cleanup: None,
            cleanup_pending: None,
            cleanup_retry: None,
            identity: PackageIdentity {
                build_id: [1; 32],
                schema_digest: [2; 32],
                helper_digest: [3; 32],
                metallib_digest: None,
            },
            selection: Selection {
                model: "system".into(),
                asset_root: None,
                endpoint: None,
                model_capabilities: None,
            },
            tools_available: false,
            inventory_only: false,
            phase,
            deadline: None,
            drain_step: 0,
            operation: [4; 16],
            generation: 1,
            codec: ModelCodec::new(),
            outgoing: VecDeque::new(),
            written: 0,
            incoming: Vec::new(),
            input: None,
            input_sent: 0,
            input_accepted: 0,
            input_granted: 0,
            snapshot: Vec::new(),
            revision: 0,
            chunks: 0,
            total_chunks: 0,
            total_bytes: 0,
            output_paused: false,
            credited: 2,
            terminal_seen: false,
            faulted: false,
            tools_enabled: false,
            pending_tool: None,
            measured_context: false,
            usage_reports: 0,
            effective_context: (phase != Phase::Hello).then_some(4096),
            thinking_controls: 0,
            completion: Default::default(),
            cleanup_completion: Default::default(),
            wake: None,
        }
    }
    fn message(body: Body) -> pb::Envelope {
        pb::Envelope {
            operation_id: Some(vec![4; 16]),
            generation: Some(1),
            body: Some(body),
        }
    }
    fn chunk(text: &str, revision: u64, ordinal: u64) -> Body {
        Body::Chunk(pb::Chunk {
            transfer_id: Some(revision + 1),
            direction: Some(2),
            ordinal: Some(ordinal),
            data: Some(text.as_bytes().to_vec()),
            revision: Some(revision),
        })
    }
    fn end(text: &str, revision: u64, count: u64) -> Body {
        end_at(text, revision, count, 0)
    }
    fn end_at(text: &str, revision: u64, count: u64, offset: u64) -> Body {
        Body::SnapshotEnd(pb::SnapshotEnd {
            revision: Some(revision),
            count: Some(count),
            total_bytes: Some(text.len() as u64),
            offset: Some(offset),
            thinking_offset: None,
            thinking_bytes: None,
        })
    }
    /// Credits queued for output transfers, in order; drains the queue.
    pub(super) fn output_credits(owner: &mut ModelOwner) -> Vec<u64> {
        let mut codec = ModelCodec::new();
        let mut credits = Vec::new();
        for frame in owner.outgoing.drain(..) {
            codec.push(&frame).unwrap();
            if let Some(Body::Credit(credit)) = codec
                .next_frame(Direction::ServiceToHelper)
                .unwrap()
                .unwrap()
                .body
                && credit.direction == Some(2)
            {
                credits.push(credit.transfer_id.unwrap());
            }
        }
        credits
    }
    /// RT-01 and RT-02: more than 1,024 revisions and more than 4 MiB of
    /// transmitted output are accepted and reconciled by the terminal.
    #[test]
    fn rt01_rt02_revisions_and_output_have_no_total() {
        let mut owner = owner(Phase::Running);
        let mut events = Vec::new();
        let piece = "x".repeat(16_384);
        let mut chunks = 0;
        let mut bytes = 0;
        for revision in 1..=1_100u64 {
            // Every tenth revision carries a full 61,440-byte transfer.
            let parts = if revision % 10 == 0 { 4 } else { 1 };
            let mut text = String::new();
            for ordinal in 0..parts {
                let data = if ordinal == 3 {
                    &piece[..12_288]
                } else {
                    &piece[..]
                };
                owner
                    .receive(message(chunk(data, revision, ordinal)), &mut events)
                    .unwrap();
                text.push_str(data);
            }
            owner
                .receive(message(end_at(&text, revision, parts, 0)), &mut events)
                .unwrap();
            owner.set_output_paused(false);
            chunks += parts;
            bytes += text.len() as u64;
            assert_eq!(output_credits(&mut owner), vec![revision + 2]);
        }
        assert!(bytes > 4 * 1024 * 1024);
        assert_eq!(events.len(), 1_100);
        assert!(matches!(
            events.last(),
            Some(ModelEvent::Revision { revision: 1_100, offset: 0, text, thinking: None }) if text.len() == 61_440
        ));
        owner
            .receive(
                message(Body::Terminal(pb::Terminal {
                    outcome: Some(1),
                    last_revision: Some(1_100),
                    count: Some(chunks),
                    total_bytes: Some(bytes),
                    usage_known: Some(false),
                    usage_tokens: None,
                    reason: Some(0),
                })),
                &mut events,
            )
            .unwrap();
        assert!(owner.terminal_seen);
    }
    /// TL-09 and TL-15: a revision's first `thinking_bytes` bytes revise the
    /// thinking text; a thinking part beyond the transfer, a lone thinking
    /// field or a part cut inside a character is a protocol fault.
    #[test]
    fn tl09_tl15_revisions_split_thinking_and_response_parts() {
        let thinking_end = |total: u64, offset: Option<u64>, bytes: Option<u64>| {
            Body::SnapshotEnd(pb::SnapshotEnd {
                revision: Some(1),
                count: Some(1),
                total_bytes: Some(total),
                offset: Some(0),
                thinking_offset: offset,
                thinking_bytes: bytes,
            })
        };
        let mut model = owner(Phase::Running);
        let mut events = Vec::new();
        model
            .receive(message(chunk("hmm…ok", 1, 0)), &mut events)
            .unwrap();
        model
            .receive(message(thinking_end(8, Some(0), Some(6))), &mut events)
            .unwrap();
        assert!(matches!(
            events.as_slice(),
            [ModelEvent::Revision { revision: 1, offset: 0, text, thinking: Some((0, thought)) }]
                if text == "ok" && thought == "hmm…"
        ));
        // A part ending inside `…` is not UTF-8 by itself.
        let mut model = owner(Phase::Running);
        model
            .receive(message(chunk("hmm…ok", 1, 0)), &mut events)
            .unwrap();
        assert!(
            model
                .receive(message(thinking_end(8, Some(0), Some(4))), &mut events)
                .is_err()
        );
        // The helper wire rejects a part beyond the transfer and a lone field.
        for (offset, bytes) in [(Some(0), Some(9)), (Some(0), None), (None, Some(2))] {
            let frame = model::encode_frame(
                &message(thinking_end(8, offset, bytes)),
                Direction::HelperToService,
            );
            assert!(frame.is_err());
        }
    }
    /// TL-15: Begin carries only a thinking request within the controls the
    /// helper reported; none is sent as an absent field.
    #[test]
    fn tl15_begin_sends_only_a_reported_thinking_form() {
        use asura_control::thinking::{self, Request};
        let input = model::encode_input(&pb::ModelInput {
            instructions: Some("i".into()),
            history: Vec::new(),
            prompt: Some("p".into()),
        })
        .unwrap();
        let begin = |controls: u32, request: Request| {
            let mut model = owner(Phase::Available);
            model.thinking_controls = controls;
            let result = model.begin([4; 16], 1, input.clone(), false, request);
            let sent = model.outgoing.front().map(|frame| {
                let mut codec = ModelCodec::new();
                codec.push(frame).unwrap();
                match codec
                    .next_frame(Direction::ServiceToHelper)
                    .unwrap()
                    .unwrap()
                    .body
                {
                    Some(Body::Begin(begin)) => begin.thinking,
                    _ => panic!("begin"),
                }
            });
            (result.is_ok(), sent.flatten())
        };
        assert_eq!(begin(0, Request::None), (true, None));
        assert_eq!(
            begin(thinking::OFF | thinking::ON, Request::On),
            (true, Some(2))
        );
        assert_eq!(begin(thinking::ALL, Request::High), (true, Some(5)));
        assert_eq!(begin(thinking::ON, Request::Off), (false, None));
        assert_eq!(begin(0, Request::Low), (false, None));
    }
    /// RT-05: while paused, the next transfer gets no credit and data for it
    /// is a fault; releasing the pause grants exactly that credit.
    #[test]
    fn rt05_paused_output_withholds_the_next_credit() {
        let mut model = owner(Phase::Running);
        let mut events = Vec::new();
        model
            .receive(message(chunk("abc", 1, 0)), &mut events)
            .unwrap();
        model
            .receive(message(end_at("abc", 1, 1, 0)), &mut events)
            .unwrap();
        // No credit until the conversation owner decides.
        assert!(output_credits(&mut model).is_empty());
        model.set_output_paused(true);
        assert!(output_credits(&mut model).is_empty());
        let mut early = owner(Phase::Running);
        early.revision = 1;
        early.output_paused = true;
        early.credited = 2;
        assert!(
            early
                .receive(message(chunk("d", 2, 0)), &mut events)
                .is_err()
        );
        model.set_output_paused(false);
        assert_eq!(output_credits(&mut model), vec![3]);
        model.set_output_paused(false);
        assert!(output_credits(&mut model).is_empty());
        model
            .receive(message(chunk("d", 2, 0)), &mut events)
            .unwrap();
        model
            .receive(message(end_at("d", 2, 1, 3)), &mut events)
            .unwrap();
        assert!(matches!(
            events.last(),
            Some(ModelEvent::Revision { revision: 2, offset: 3, text, thinking: None }) if text == "d"
        ));
    }
    /// B3: tool availability comes from `supported_capabilities` bit 0 alone.
    #[test]
    fn tool_availability_comes_from_the_capability_mask_alone() {
        for supported in [None, Some(0), Some(1), Some(4), Some(5)] {
            let mut owner = owner(Phase::Hello);
            let hello = pb::Envelope {
                body: Some(Body::Hello(pb::Hello {
                    build_id: Some(vec![1; 32]),
                    schema_digest: Some(vec![2; 32]),
                    selected_model: Some("system".into()),
                    availability: Some(1),
                    context_tokens: Some(4096),
                    reported_context_tokens: Some(4096),
                    context_source: Some(1),
                    supported_capabilities: supported,
                    capability_source: supported.map(|_| 1),
                    ..Default::default()
                })),
                ..Default::default()
            };
            owner.receive(hello, &mut Vec::new()).unwrap();
            assert_eq!(
                owner.tools_available,
                supported.is_some_and(|mask| mask & 1 != 0)
            );
        }
    }

    #[test]
    fn ollama_tool_permission_requires_both_local_endpoint_and_helper_attestation() {
        let mut selection = owner(Phase::Available).selection;
        selection.model = "ollama:fixture".into();
        assert!(!local_tool_destination(&selection, false));
        assert!(local_tool_destination(&selection, true));
        for endpoint in [
            "https://remote.example",
            "http://127.0.0.1.evil:11434",
            "http://user@127.0.0.1",
            "http://localhost:11434",
        ] {
            selection.endpoint = Some(endpoint.into());
            assert!(!local_tool_destination(&selection, true));
        }
        selection.endpoint = Some("http://[::1]:11434/".into());
        assert!(local_tool_destination(&selection, true));
    }

    #[test]
    fn remote_ollama_endpoints_are_not_permitted_for_turns() {
        assert!(endpoint_permitted("ollama", None));
        assert!(endpoint_permitted("ollama", Some("http://127.0.0.1:11434")));
        assert!(endpoint_permitted("ollama", Some("https://[::1]:8443")));
        for endpoint in [
            "https://models.example",
            "https://localhost",
            "https://10.0.0.1",
        ] {
            assert!(!endpoint_permitted("ollama", Some(endpoint)), "{endpoint}");
        }
        assert!(endpoint_permitted("system", Some("https://models.example")));
    }

    #[test]
    fn begin_intersects_admitted_tools_with_verified_native_capability() {
        for admitted in [false, true] {
            for available in [false, true] {
                let mut owner = owner(Phase::Available);
                owner.tools_available = available;
                let input = model::encode_input(&pb::ModelInput {
                    instructions: Some(String::new()),
                    history: vec![],
                    prompt: Some("hello".into()),
                })
                .unwrap();
                owner
                    .begin(
                        [4; 16],
                        1,
                        input,
                        admitted,
                        asura_control::thinking::Request::None,
                    )
                    .unwrap();
                assert_eq!(owner.tools_enabled, admitted && available);
                assert_eq!(owner.phase, Phase::Input);
            }
        }
    }

    #[test]
    fn missing_native_tools_does_not_hide_unready_provider() {
        for phase in [Phase::Preparing, Phase::Hello, Phase::Draining] {
            let mut owner = owner(phase);
            owner.tools_available = false;
            assert!(
                owner
                    .begin(
                        [4; 16],
                        1,
                        vec![],
                        true,
                        asura_control::thinking::Request::None
                    )
                    .is_err()
            );
            assert!(owner.outgoing.is_empty());
        }
    }

    #[test]
    fn running_generation_does_not_expire_with_elapsed_time() {
        let mut owner = owner(Phase::Running);
        let now = Instant::now();
        assert_eq!(owner.next_deadline(now), None);
        assert!(owner.poll(now + Duration::from_secs(120)).is_empty());
        assert_eq!(owner.phase, Phase::Running);
        owner.cancel(1, now);
        assert_eq!(owner.phase, Phase::Draining);
        assert_eq!(owner.deadline, Some(now + Duration::from_millis(250)));
        assert_eq!(
            owner.next_deadline(now),
            None,
            "no child remains in this unit fixture"
        );
    }

    #[test]
    /// HW-01: each handshake step, held past its former limit (20 s
    /// preparation; 5 s Hello, Available-to-Begin and Ready-to-Start) with an
    /// injected clock, stays pending; no timer fails it. HW-02: cancellation
    /// ends each step and starts only the wind-down.
    fn hw01_each_step_waits_without_a_time_limit_until_cancel() {
        for phase in [
            Phase::Preparing,
            Phase::Hello,
            Phase::Available,
            Phase::Input,
            Phase::Ready,
        ] {
            let mut owner = owner(phase);
            let now = Instant::now();
            assert_eq!(owner.next_deadline(now), None, "{phase:?}");
            assert!(
                owner.poll(now + Duration::from_secs(3600)).is_empty(),
                "{phase:?}"
            );
            assert_eq!(owner.phase, phase);
            assert!(!owner.cancel_preparation.load(Ordering::Acquire));
            owner.cancel(3, now);
            assert_eq!(owner.phase, Phase::Draining);
            assert!(owner.cancel_preparation.load(Ordering::Acquire));
            assert_eq!(owner.deadline, Some(now + WIND_DOWN_GRACE));
        }
        // Available -> Begin and Ready -> Start accept the late step.
        let mut owner = owner(Phase::Ready);
        assert!(owner.start().is_ok());
        assert_eq!(owner.phase, Phase::Running);
    }
    /// WS-01: the helper step follows the owner's own phase transitions and
    /// disappears on cancel; it arms no timer.
    #[test]
    fn ws01_helper_step_tracks_phase_and_cancel_removes_it() {
        for (phase, step) in [
            (Phase::Preparing, Some(HelperStep::Preparing)),
            (Phase::Hello, Some(HelperStep::Hello)),
            (Phase::Available, None),
            (Phase::Input, Some(HelperStep::Input)),
            (Phase::Ready, None),
            (Phase::Running, Some(HelperStep::Running)),
            (Phase::Draining, None),
            (Phase::Settled, None),
        ] {
            let owner = owner(phase);
            assert_eq!(owner.helper_step(), step, "{phase:?}");
            assert_eq!(owner.next_deadline(Instant::now()), None, "{phase:?}");
        }
        let mut owner = owner(Phase::Available);
        let input = model::encode_input(&pb::ModelInput {
            instructions: Some(String::new()),
            history: vec![],
            prompt: Some("hello".into()),
        })
        .unwrap();
        owner
            .begin(
                [4; 16],
                1,
                input,
                false,
                asura_control::thinking::Request::None,
            )
            .unwrap();
        assert_eq!(owner.helper_step(), Some(HelperStep::Input));
        let mut owner = owner_with_ready();
        assert_eq!(owner.helper_step(), None);
        owner.start().unwrap();
        assert_eq!(owner.helper_step(), Some(HelperStep::Running));
        owner.cancel(1, Instant::now());
        assert_eq!(owner.helper_step(), None);
    }
    fn owner_with_ready() -> ModelOwner {
        owner(Phase::Ready)
    }
    /// HW-03: Begin never sends `deadline_remaining_ms`; UH13: nor an output
    /// cap (owner, 2026-10-19).
    #[test]
    fn hw03_begin_sends_no_generation_deadline_or_output_cap() {
        let mut owner = owner(Phase::Available);
        let input = model::encode_input(&pb::ModelInput {
            instructions: Some(String::new()),
            history: vec![],
            prompt: Some("hello".into()),
        })
        .unwrap();
        owner
            .begin(
                [4; 16],
                1,
                input,
                false,
                asura_control::thinking::Request::None,
            )
            .unwrap();
        let frame = owner.outgoing.front().unwrap().clone();
        let mut codec = ModelCodec::new();
        codec.push(&frame).unwrap();
        let envelope = codec
            .next_frame(Direction::ServiceToHelper)
            .unwrap()
            .unwrap();
        let Some(Body::Begin(begin)) = envelope.body else {
            panic!("expected Begin")
        };
        // Begin has no time field and no output cap; the whole message is
        // the model, the input size and the tool flag.
        assert_eq!(
            begin,
            pb::Begin {
                model: begin.model.clone(),
                input_bytes: Some(begin.input_bytes.unwrap()),
                enable_project_tools: None,
                thinking: None,
            }
        );
        assert!(begin.model.is_some());
    }

    #[test]
    fn snapshot_replaces_and_terminal_reconciles_cumulative_bytes() {
        let mut owner = owner(Phase::Running);
        let mut events = Vec::new();
        for (rev, text) in [(1, "a"), (2, "a complete response")] {
            owner
                .receive(message(chunk(text, rev, 0)), &mut events)
                .unwrap();
            owner
                .receive(message(end(text, rev, 1)), &mut events)
                .unwrap();
            owner.set_output_paused(false);
        }
        assert!(
            matches!(&events[1],ModelEvent::Revision{revision:2,text,..} if text=="a complete response")
        );
        let terminal = pb::Terminal {
            outcome: Some(1),
            last_revision: Some(2),
            count: Some(2),
            total_bytes: Some(20),
            usage_known: Some(true),
            usage_tokens: Some(3),
            reason: Some(0),
        };
        owner
            .receive(message(Body::Terminal(terminal)), &mut events)
            .unwrap();
        assert!(owner.terminal_seen);
        assert!(
            owner
                .receive(message(chunk("late", 3, 0)), &mut events)
                .is_err()
        );
    }
    #[test]
    fn invalid_identity_ordinal_and_counts_reject_without_publication() {
        let mut owner = owner(Phase::Running);
        let mut events = Vec::new();
        let mut wrong = message(chunk("x", 1, 0));
        wrong.generation = Some(2);
        assert!(owner.receive(wrong, &mut events).is_err());
        assert!(
            owner
                .receive(message(chunk("x", 1, 1)), &mut events)
                .is_err()
        );
        owner
            .receive(message(chunk("x", 1, 0)), &mut events)
            .unwrap();
        assert!(
            owner
                .receive(message(end("wrong", 1, 1)), &mut events)
                .is_err()
        );
        assert!(events.is_empty());
    }
    #[test]
    fn cancelled_tool_result_never_enters_output_queue() {
        let mut owner = owner(Phase::Running);
        owner.tools_enabled = true;
        owner.pending_tool = Some(1);
        owner.cancel(1, Instant::now());
        owner.outgoing.clear();
        assert!(
            owner
                .tool_result(pb::ToolResult {
                    ordinal: Some(1),
                    status: Some(1),
                    text: Some("secret".into()),
                    next_offset: None,
                    truncated: Some(false)
                })
                .is_err()
        );
        assert!(owner.outgoing.is_empty());
        assert_eq!(owner.pending_tool, Some(1));
    }
    #[test]
    fn cancellation_discards_unstarted_result_and_abandons_partial_frame() {
        for partial in [false, true] {
            let mut owner = owner(Phase::Running);
            owner.tools_enabled = true;
            owner.pending_tool = Some(1);
            owner
                .tool_result(pb::ToolResult {
                    ordinal: Some(1),
                    status: Some(1),
                    text: Some("secret".into()),
                    next_offset: None,
                    truncated: Some(false),
                })
                .unwrap();
            let result_frame = owner.outgoing.front().unwrap().clone();
            owner.written = usize::from(partial);
            owner.cancel(1, Instant::now());
            assert_eq!(owner.written, 0);
            assert!(!owner.outgoing.iter().any(|frame| frame == &result_frame));
            assert_eq!(owner.outgoing.len(), usize::from(!partial));
            assert_eq!(owner.phase, Phase::Draining);
        }
    }
    #[test]
    fn cancellation_prevents_start_and_retains_generation() {
        let mut owner = owner(Phase::Ready);
        owner.cancel(1, Instant::now());
        assert!(owner.start().is_err());
        assert_eq!(owner.generation, 1);
        assert_eq!(owner.outgoing.len(), 1);
        owner.poll(Instant::now());
        assert!(owner.is_settled());
    }
    #[test]
    fn input_requires_single_credit_and_ready_then_explicit_start() {
        let mut owner = owner(Phase::Available);
        let input = model::encode_input(&pb::ModelInput {
            instructions: Some(String::new()),
            history: vec![],
            prompt: Some("hello".into()),
        })
        .unwrap();
        owner
            .begin(
                [4; 16],
                1,
                input,
                false,
                asura_control::thinking::Request::None,
            )
            .unwrap();
        assert!(owner.start().is_err());
        let mut events = Vec::new();
        let credit = message(Body::Credit(pb::Credit {
            transfer_id: Some(1),
            direction: Some(1),
            accepted_bytes: Some(0),
            granted_bytes: Some(65536),
        }));
        owner.receive(credit.clone(), &mut events).unwrap();
        assert!(owner.receive(credit, &mut events).is_err());
        owner
            .receive(message(Body::Ready(pb::Ready {})), &mut events)
            .unwrap();
        // The Ready-to-Start handoff arms no timer (owner, 2026-10-16).
        assert_eq!(owner.deadline, None);
        owner.start().unwrap();
        assert!(owner.start().is_err());
    }
    /// CC-U16 and CC-U18: input above 64 KiB streams under a sliding credit
    /// window; nothing is sent beyond the grant; the capacity bound applies.
    #[test]
    fn window_sized_input_streams_under_sliding_credit() {
        let mut owner = owner(Phase::Available);
        owner.effective_context = Some(100_000);
        let input = model::encode_input(&pb::ModelInput {
            instructions: Some("x".repeat(200_000)),
            history: vec![],
            prompt: Some("hello".into()),
        })
        .unwrap();
        let total = input.len() as u64;
        owner
            .begin(
                [4; 16],
                1,
                input,
                false,
                asura_control::thinking::Request::None,
            )
            .unwrap();
        let credit = |accepted: u64, granted: u64| {
            message(Body::Credit(pb::Credit {
                transfer_id: Some(1),
                direction: Some(1),
                accepted_bytes: Some(accepted),
                granted_bytes: Some(granted),
            }))
        };
        let mut events = Vec::new();
        owner.receive(credit(0, 65_536), &mut events).unwrap();
        assert_eq!(owner.input_sent, 65_536);
        // A window wider than 64 KiB, or acceptance of unsent bytes, is a fault.
        assert!(owner.clone_ledger_check(credit(0, 131_073)));
        assert!(owner.clone_ledger_check(credit(65_537, 131_072)));
        let mut accepted = 0;
        while owner.input.is_some() {
            // The I/O pass writes queued frames to the helper; drain them here.
            owner.outgoing.clear();
            accepted += 16_384;
            let granted = accepted + 65_536;
            owner
                .receive(credit(accepted, granted), &mut events)
                .unwrap();
            assert!(owner.input_sent <= granted);
        }
        assert_eq!(owner.input_sent, total);
        // The helper credits every accepted chunk, so credits for chunks
        // already sent follow InputEnd; they queue nothing and are not faults.
        owner.outgoing.clear();
        while accepted + 16_384 <= total {
            accepted += 16_384;
            owner
                .receive(credit(accepted, accepted + 65_536), &mut events)
                .unwrap();
        }
        assert!(owner.outgoing.is_empty(), "nothing is sent after InputEnd");
        // Acceptance of unsent bytes is still a fault after InputEnd.
        assert!(owner.clone_ledger_check(credit(total + 1, total + 65_537)));
        owner
            .receive(message(Body::Ready(pb::Ready {})), &mut events)
            .unwrap();
        owner.start().unwrap();
        let mut owner = owner_with_capacity(100);
        let input = model::encode_input(&pb::ModelInput {
            instructions: Some("x".repeat(801)),
            history: vec![],
            prompt: Some("hello".into()),
        })
        .unwrap();
        assert!(
            owner
                .begin(
                    [4; 16],
                    1,
                    input,
                    false,
                    asura_control::thinking::Request::None
                )
                .is_err()
        );
    }
    #[test]
    fn cancel_during_transfer_stops_sending() {
        let mut owner = owner(Phase::Available);
        owner.effective_context = Some(100_000);
        let input = model::encode_input(&pb::ModelInput {
            instructions: Some("x".repeat(200_000)),
            history: vec![],
            prompt: Some("hello".into()),
        })
        .unwrap();
        owner
            .begin(
                [4; 16],
                1,
                input,
                false,
                asura_control::thinking::Request::None,
            )
            .unwrap();
        let mut events = Vec::new();
        owner
            .receive(
                message(Body::Credit(pb::Credit {
                    transfer_id: Some(1),
                    direction: Some(1),
                    accepted_bytes: Some(0),
                    granted_bytes: Some(65_536),
                })),
                &mut events,
            )
            .unwrap();
        owner.cancel(1, Instant::now());
        let sent = owner.input_sent;
        let _ = owner.receive(
            message(Body::Credit(pb::Credit {
                transfer_id: Some(1),
                direction: Some(1),
                accepted_bytes: Some(16_384),
                granted_bytes: Some(81_920),
            })),
            &mut events,
        );
        assert_eq!(owner.input_sent, sent);
    }
    #[test]
    fn output_limit_is_checked_before_snapshot_allocation() {
        let mut owner = owner(Phase::Running);
        let mut events = Vec::new();
        for n in 0..3 {
            owner
                .receive(message(chunk(&"x".repeat(16384), 1, n)), &mut events)
                .unwrap();
        }
        assert!(
            owner
                .receive(message(chunk(&"x".repeat(16384), 1, 3)), &mut events)
                .is_err()
        );
        assert_eq!(owner.snapshot.len(), 49152);
        assert!(events.is_empty());
    }
    #[test]
    fn cancel_reservation_is_bounded() {
        let mut owner = owner(Phase::Ready);
        for _ in 0..7 {
            owner.credit(2).unwrap();
        }
        assert!(owner.credit(2).is_err());
        owner.cancel(1, Instant::now());
        assert_eq!(
            owner.outgoing.len(),
            1,
            "cancellation must purge stale queued work"
        );
        let expected = model::encode_frame(
            &message(Body::Cancel(pb::Cancel { reason: Some(1) })),
            Direction::ServiceToHelper,
        )
        .unwrap();
        assert_eq!(owner.outgoing.front(), Some(&expected));
        assert!(owner.outgoing.iter().all(|v| v.len() <= 4096));
    }
    #[test]
    fn inventory_hello_returns_metadata_without_admitting_generation() {
        let mut owner = owner(Phase::Hello);
        owner.inventory_only = true;
        owner.generation = 0;
        owner.operation = [0; 16];
        let mut events = Vec::new();
        let hello = pb::Envelope {
            operation_id: None,
            generation: None,
            body: Some(Body::Hello(pb::Hello {
                local_tool_destination: None,
                tool_schema_bytes: None,
                thinking_controls: None,
                supported_capabilities: None,
                capability_source: None,
                model_capabilities: None,
                build_id: Some(vec![1; 32]),
                schema_digest: Some(vec![2; 32]),
                selected_model: Some("system".into()),
                inventory_only: Some(true),
                availability: Some(2),
                models: vec![pb::ModelInventoryEntry {
                    selector: Some("system".into()),
                    provider: Some("system".into()),
                    status: Some(4),
                    detail: None,
                }],
                ..Default::default()
            })),
        };
        owner.receive(hello, &mut events).unwrap();
        assert!(matches!(events.as_slice(), [ModelEvent::Inventory { .. }]));
        assert_eq!(owner.phase, Phase::Draining);
        assert!(
            owner
                .begin(
                    [7; 16],
                    1,
                    vec![],
                    false,
                    asura_control::thinking::Request::None
                )
                .is_err()
        );
        assert!(owner.outgoing.is_empty());
    }
    #[test]
    fn hello_identity_is_checked_before_availability_is_published() {
        let mut owner = owner(Phase::Hello);
        let mut events = Vec::new();
        let hello = pb::Envelope {
            operation_id: None,
            generation: None,
            body: Some(Body::Hello(pb::Hello {
                local_tool_destination: None,
                tool_schema_bytes: None,
                thinking_controls: None,
                supported_capabilities: None,
                capability_source: None,
                model_capabilities: None,
                inventory_only: None,
                models: Vec::new(),
                issues: Vec::new(),
                selected_model: None,
                asset_root: None,
                endpoint: None,
                build_id: Some(vec![9; 32]),
                schema_digest: Some(vec![2; 32]),
                max_frame_bytes: Some(65536),
                availability: Some(1),
                context_tokens: Some(4096),
                reported_context_tokens: Some(4096),
                context_source: Some(1),
                model_name: None,
                reason: Some(0),
            })),
        };
        assert!(owner.receive(hello, &mut events).is_err());
        assert!(events.is_empty());
    }
    #[test]
    fn protocol_failure_fences_late_success() {
        let mut owner = owner(Phase::Running);
        let mut events = Vec::new();
        owner.fail("model_protocol_fault", Instant::now(), &mut events);
        owner
            .receive(
                message(Body::Terminal(pb::Terminal {
                    outcome: Some(1),
                    last_revision: Some(1),
                    count: Some(1),
                    total_bytes: Some(1),
                    usage_tokens: Some(1),
                    usage_known: Some(true),
                    reason: Some(0),
                })),
                &mut events,
            )
            .unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            ModelEvent::Failed("model_protocol_fault")
        ));
        assert!(!owner.terminal_seen);
    }
    #[test]
    fn completion_cannot_overtake_a_pending_durable_tool_result() {
        let mut owner = owner(Phase::Running);
        owner.tools_enabled = true;
        let mut events = Vec::new();
        owner
            .receive(message(chunk("answer", 1, 0)), &mut events)
            .unwrap();
        owner
            .receive(message(end("answer", 1, 1)), &mut events)
            .unwrap();
        owner
            .receive(
                message(Body::ToolCall(pb::ToolCall {
                    ordinal: Some(1),
                    name: Some("project".into()),
                    arguments: Some(r#"{"command":"list_directory"}"#.into()),
                    arguments_bytes: None,
                    name_bytes: None,
                    source: Some(1),
                })),
                &mut events,
            )
            .unwrap();
        let terminal = pb::Terminal {
            outcome: Some(1),
            last_revision: Some(1),
            count: Some(1),
            total_bytes: Some(6),
            usage_known: Some(false),
            usage_tokens: None,
            reason: Some(0),
        };
        assert_eq!(
            owner.receive(message(Body::Terminal(terminal)), &mut events),
            Err("model_protocol_fault")
        );
        assert!(!owner.terminal_seen);
        owner
            .tool_result(pb::ToolResult {
                ordinal: Some(1),
                status: Some(1),
                text: Some("file".into()),
                next_offset: None,
                truncated: Some(false),
            })
            .unwrap();
        owner
            .receive(message(Body::Terminal(terminal)), &mut events)
            .unwrap();
        assert!(owner.terminal_seen);
    }
    #[test]
    fn measured_input_context_is_once_before_output_and_generation_fenced() {
        let mut owner = owner(Phase::Running);
        let mut events = Vec::new();
        let body = Body::ContextMeasured(pb::ContextMeasured {
            input_tokens: Some(120),
            capacity_tokens: Some(4096),
        });
        let mut wrong = message(body.clone());
        wrong.generation = Some(2);
        assert!(owner.receive(wrong, &mut events).is_err());
        owner.receive(message(body.clone()), &mut events).unwrap();
        assert!(matches!(
            events.as_slice(),
            [ModelEvent::ContextMeasured {
                input_tokens: 120,
                capacity_tokens: 4096
            }]
        ));
        assert!(owner.receive(message(body), &mut events).is_err());
    }
    #[test]
    fn measured_capacity_must_match_discovered_effective_window() {
        let mut owner = owner(Phase::Running);
        let events = &mut Vec::new();
        assert!(
            owner
                .receive(
                    message(Body::ContextMeasured(pb::ContextMeasured {
                        input_tokens: Some(120),
                        capacity_tokens: Some(8192),
                    })),
                    events
                )
                .is_err()
        );
        assert!(events.is_empty());
    }

    #[test]
    fn measured_context_after_output_preserves_response_and_rejects_duplicates() {
        let mut owner = owner(Phase::Running);
        let mut events = Vec::new();
        owner
            .receive(message(chunk("text", 1, 0)), &mut events)
            .unwrap();
        owner
            .receive(message(end("text", 1, 1)), &mut events)
            .unwrap();
        let measured = Body::ContextMeasured(pb::ContextMeasured {
            input_tokens: Some(120),
            capacity_tokens: Some(4096),
        });
        owner
            .receive(message(measured.clone()), &mut events)
            .unwrap();
        assert!(matches!(&events[0], ModelEvent::Revision { text, .. } if text == "text"));
        assert!(matches!(
            &events[1],
            ModelEvent::ContextMeasured {
                input_tokens: 120,
                capacity_tokens: 4096
            }
        ));
        assert!(owner.receive(message(measured), &mut events).is_err());
    }
    #[test]
    fn coalesced_frames_rearm_until_terminal_consumed() {
        let mut owner = owner(Phase::Running);
        // One credited revision of nine one-byte chunks: more than the eight
        // frames one pass consumes. Each later revision needs a new credit.
        let bodies =
            (0..9)
                .map(|ordinal| chunk("x", 1, ordinal))
                .chain([end(&"x".repeat(9), 1, 9)]);
        for body in bodies {
            owner
                .incoming
                .extend(model::encode_frame(&message(body), Direction::HelperToService).unwrap());
        }
        owner.incoming.extend(
            model::encode_frame(
                &message(Body::Terminal(pb::Terminal {
                    outcome: Some(1),
                    last_revision: Some(1),
                    count: Some(9),
                    total_bytes: Some(9),
                    usage_tokens: None,
                    usage_known: Some(false),
                    reason: Some(0),
                })),
                Direction::HelperToService,
            )
            .unwrap(),
        );
        let mut events = Vec::new();
        owner.consume_input(&mut events).unwrap();
        assert!(events.is_empty());
        assert_eq!(owner.chunks, 8);
        let now = Instant::now();
        assert_eq!(owner.next_deadline(now), Some(now));
        owner.consume_input(&mut events).unwrap();
        assert!(
            matches!(&events[0], ModelEvent::Revision { revision: 1, text, .. } if text.len() == 9)
        );
        assert!(owner.terminal_seen);
        assert!(!owner.buffered_work());
    }
    #[test]
    fn cancelled_preparation_does_not_spin_on_expired_deadline() {
        let mut owner = owner(Phase::Preparing);
        let now = Instant::now();
        owner.cancel(1, now);
        assert_eq!(owner.next_deadline(now + Duration::from_secs(1)), None);
        owner.completion.finish();
        // A retained preparation handle is what permits a settlement timer.
        let task = thread::spawn(|| Err(asura_platform::Error::Unavailable));
        owner.preparation = Some(task);
        assert!(
            owner.next_deadline(now + Duration::from_secs(1)).unwrap()
                > now + Duration::from_secs(1)
        );
        assert!(owner.preparation.take().unwrap().join().unwrap().is_err());
    }
    #[test]
    fn failed_cleanup_spawn_has_explicit_retry_deadline() {
        let mut owner = owner(Phase::Draining);
        let now = Instant::now();
        owner.cleanup_pending = Some(Arc::new(Mutex::new(None)));
        owner.cleanup_retry = Some(now + Duration::from_millis(100));
        owner.deadline = Some(now - Duration::from_secs(1));
        assert_eq!(owner.next_deadline(now), owner.cleanup_retry);
    }
    #[test]
    fn request_usage_is_ordered_once_and_preserves_output_text() {
        let mut owner = owner(Phase::Running);
        let mut events = Vec::new();
        owner
            .receive(
                message(Body::InferenceUsage(pb::InferenceUsage {
                    request_ordinal: Some(1),
                    input_tokens: Some(0),
                    output_tokens: Some(3),
                    complete: Some(true),
                    ..Default::default()
                })),
                &mut events,
            )
            .unwrap();
        assert!(
            matches!(events.as_slice(), [ModelEvent::InferenceUsage(v)] if v.output_tokens == Some(3))
        );
        assert_eq!(owner.revision, 0);
        assert!(
            owner
                .receive(
                    message(Body::InferenceUsage(pb::InferenceUsage {
                        request_ordinal: Some(1),
                        complete: Some(false),
                        ..Default::default()
                    })),
                    &mut events
                )
                .is_err()
        );
        assert!(
            owner
                .receive(
                    message(Body::InferenceUsage(pb::InferenceUsage {
                        request_ordinal: Some(3),
                        complete: Some(false),
                        ..Default::default()
                    })),
                    &mut events
                )
                .is_err()
        );
        owner
            .receive(
                message(Body::InferenceUsage(pb::InferenceUsage {
                    request_ordinal: Some(2),
                    complete: Some(false),
                    ..Default::default()
                })),
                &mut events,
            )
            .unwrap();
    }
}
