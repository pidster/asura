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
        capabilities: crate::model::CapabilityProfile,
    },
    ContextMeasured {
        input_tokens: u32,
        capacity_tokens: u32,
    },
    Ready,
    Snapshot {
        revision: u64,
        text: String,
    },
    Terminal {
        outcome: i32,
        reason: i32,
        usage_tokens: Option<u64>,
    },
    ToolCall(pb::ToolCall),
    Failed(&'static str),
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
    deadline: Instant,
    drain_step: u8,
    operation: [u8; 16],
    generation: u64,
    codec: ModelCodec,
    outgoing: VecDeque<Vec<u8>>,
    written: usize,
    incoming: Vec<u8>,
    input: Option<Vec<u8>>,
    snapshot: Vec<u8>,
    revision: u64,
    chunks: u64,
    total_chunks: u64,
    total_bytes: u64,
    terminal_seen: bool,
    faulted: bool,
    tools_enabled: bool,
    pending_tool: Option<u32>,
    measured_context: bool,
    effective_context: Option<u32>,
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
        (self.phase != Phase::Running && (self.child.is_some() || self.phase != Phase::Draining))
            .then_some(self.deadline)
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
        now: Instant,
    ) -> io::Result<Self> {
        Self::create(runtime, identity, selection, false, now)
    }
    pub(crate) fn inventory(
        runtime: RuntimeDirectory,
        identity: PackageIdentity,
        selection: Selection,
        now: Instant,
    ) -> io::Result<Self> {
        Self::create(runtime, identity, selection, true, now)
    }
    fn create(
        runtime: RuntimeDirectory,
        identity: PackageIdentity,
        selection: Selection,
        inventory_only: bool,
        now: Instant,
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
        let deadline = now + Duration::from_secs(20);
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
                    deadline,
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
            deadline,
            drain_step: 0,
            operation: [0; 16],
            generation: 0,
            codec: ModelCodec::new(),
            outgoing: VecDeque::new(),
            written: 0,
            incoming: Vec::new(),
            input: None,
            snapshot: Vec::new(),
            revision: 0,
            chunks: 0,
            total_chunks: 0,
            total_bytes: 0,
            terminal_seen: false,
            faulted: false,
            tools_enabled: false,
            pending_tool: None,
            measured_context: false,
            effective_context: None,
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
        now: Instant,
    ) -> Result<(), &'static str> {
        if self.phase != Phase::Available
            || now >= self.deadline
            || generation == 0
            || operation == [0; 16]
        {
            return Err("model_not_ready");
        }
        model::decode_input(&input).map_err(|_| "model_input_invalid")?;
        // Absence of native tool support is negotiated before inference. A real
        // provider failure never retries the turn without tools.
        let enable_tools = enable_tools && self.tools_available;
        self.tools_enabled = enable_tools;
        self.operation = operation;
        self.generation = generation;
        self.queue(Body::Begin(pb::Begin {
            model: Some(self.selection.model.clone()),
            input_bytes: Some(input.len() as u64),
            deadline_remaining_ms: None,
            max_response_tokens: Some(asura_storage::authority::conversation::OUTPUT_RESERVATION),
            enable_project_tools: enable_tools.then_some(true),
        }))?;
        self.input = Some(input);
        self.phase = Phase::Input;
        self.deadline = now + Duration::from_secs(5);
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
        if self.phase != Phase::Ready || Instant::now() >= self.deadline {
            return Err("model_not_ready");
        }
        self.queue(Body::Start(pb::Start {}))?;
        self.credit(2)?;
        self.phase = Phase::Running;
        Ok(())
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
        self.deadline = now + Duration::from_millis(250);
        self.drain_step = 0;
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
                        self.deadline = now + Duration::from_secs(5);
                        let hello = Body::Hello(pb::Hello {
                            local_tool_destination: None,
                            reasoning_disabled: None,
                            supported_capabilities: None,
                            capability_source: None,
                            model_capabilities: self.selection.model_capabilities,
                            selected_model: Some(self.selection.model.clone()),
                            asset_root: self.selection.asset_root.clone(),
                            endpoint: self.selection.endpoint.clone(),
                            build_id: Some(self.identity.build_id.to_vec()),
                            schema_digest: Some(self.identity.schema_digest.to_vec()),
                            max_frame_bytes: Some(65536),
                            availability: Some(3),
                            capabilities: Some(0),
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
                        Ok(Err(asura_platform::Error::Deadline)) => ("preparation_timeout", 0),
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
        if now >= self.deadline
            && !matches!(
                self.phase,
                Phase::Running | Phase::Settled | Phase::Draining
            )
        {
            self.fail("model_timeout", now, &mut events);
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
                self.deadline = now + Duration::from_millis(250);
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
                } else if self.phase == Phase::Draining && now >= self.deadline {
                    let _ = child.terminate(self.drain_step > 0);
                    self.drain_step = self.drain_step.saturating_add(1);
                    self.deadline = now
                        + if self.drain_step == 1 {
                            Duration::from_millis(250)
                        } else {
                            Duration::from_secs(1)
                        };
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
            if hello.selected_model.as_deref().unwrap_or("system") != self.selection.model {
                return Err("model_protocol_identity_mismatch");
            }
            // The canonical codec validates the source against the selector,
            // including case-insensitive provider prefixes. Identity stays exact.
            self.effective_context = hello.context_tokens;
            self.tools_available = hello.capabilities == Some(3)
                && hello
                    .supported_capabilities
                    .is_some_and(|mask| mask & 1 != 0)
                && matches!(hello.capability_source, Some(1..=3))
                && local_tool_destination(
                    &self.selection,
                    hello.local_tool_destination == Some(true),
                );
            self.phase = Phase::Available;
            self.deadline = Instant::now() + Duration::from_secs(5);
            events.push(ModelEvent::Available {
                context_tokens: hello.context_tokens.unwrap(),
                reported_context_tokens: hello.reported_context_tokens.unwrap(),
                context_source: hello.context_source.unwrap(),
                model_name: hello.model_name,
                tools_available: self.tools_available,
                capabilities: crate::model::CapabilityProfile {
                    supported: hello.supported_capabilities,
                    source: hello.capability_source,
                    reasoning_disabled: hello.reasoning_disabled == Some(true),
                },
            });
            return Ok(());
        }
        model::validate_identity(&envelope, &self.operation, self.generation)
            .map_err(|_| "model_protocol_identity_mismatch")?;
        match envelope.body.ok_or("model_protocol_fault")? {
            Body::Credit(credit) if self.phase == Phase::Input => {
                let input = self.input.take().ok_or("model_protocol_fault")?;
                if credit.transfer_id != Some(1)
                    || credit.accepted_bytes != Some(0)
                    || credit.granted_bytes != Some(65536)
                {
                    return Err("model_protocol_fault");
                }
                let count = input.len().div_ceil(16384);
                for (ordinal, chunk) in input.chunks(16384).enumerate() {
                    self.queue(Body::Chunk(pb::Chunk {
                        transfer_id: Some(1),
                        direction: Some(1),
                        ordinal: Some(ordinal as u64),
                        data: Some(chunk.to_vec()),
                        revision: Some(0),
                    }))?;
                }
                self.queue(Body::InputEnd(pb::InputEnd {
                    count: Some(count as u64),
                    total_bytes: Some(input.len() as u64),
                }))?;
            }
            Body::Ready(_) if self.phase == Phase::Input && self.input.is_none() => {
                self.phase = Phase::Ready;
                self.deadline = Instant::now() + Duration::from_secs(5);
                events.push(ModelEvent::Ready);
            }
            Body::ToolCall(call) if self.phase == Phase::Running && self.tools_enabled => {
                if self.pending_tool.is_some() && self.pending_tool != call.ordinal {
                    return Err("model_protocol_fault");
                }
                self.pending_tool = call.ordinal;
                events.push(ModelEvent::ToolCall(call));
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
                if chunk.transfer_id != Some(self.revision + 2)
                    || chunk.revision != Some(self.revision + 1)
                    || chunk.ordinal != Some(self.chunks)
                {
                    return Err("model_protocol_fault");
                }
                let data = chunk.data.unwrap();
                if self.snapshot.len() + data.len() > 60 * 1024
                    || self.total_bytes + data.len() as u64 > 4 * 1024 * 1024
                {
                    return Err("model_output_limit");
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
                let text = String::from_utf8(std::mem::take(&mut self.snapshot))
                    .map_err(|_| "model_protocol_fault")?;
                if self.phase == Phase::Running {
                    events.push(ModelEvent::Snapshot {
                        revision: self.revision,
                        text,
                    });
                    if self.revision < 1024 {
                        self.credit(self.revision + 2)?;
                    }
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
    let endpoint = selection
        .endpoint
        .as_deref()
        .unwrap_or("http://127.0.0.1:11434");
    let authority = endpoint
        .strip_prefix("http://")
        .or_else(|| endpoint.strip_prefix("https://"))
        .and_then(|rest| rest.strip_suffix('/').or(Some(rest)));
    authority.is_some_and(|host| {
        ["127.0.0.1", "[::1]"].iter().any(|ip| {
            host == *ip
                || host.strip_prefix(ip).is_some_and(|port| {
                    port.strip_prefix(':')
                        .and_then(|port| port.parse::<u16>().ok())
                        .is_some_and(|port| port > 0)
                })
        })
    })
}

#[cfg(test)]
impl ModelOwner {
    pub(crate) fn tool_test_fixture() -> Self {
        let mut value = tests::owner(Phase::Running);
        value.tools_enabled = true;
        value.pending_tool = Some(1);
        value
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
            deadline: Instant::now() + Duration::from_secs(60),
            drain_step: 0,
            operation: [4; 16],
            generation: 1,
            codec: ModelCodec::new(),
            outgoing: VecDeque::new(),
            written: 0,
            incoming: Vec::new(),
            input: None,
            snapshot: Vec::new(),
            revision: 0,
            chunks: 0,
            total_chunks: 0,
            total_bytes: 0,
            terminal_seen: false,
            faulted: false,
            tools_enabled: false,
            pending_tool: None,
            measured_context: false,
            effective_context: (phase != Phase::Hello).then_some(4096),
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
        Body::SnapshotEnd(pb::SnapshotEnd {
            revision: Some(revision),
            count: Some(count),
            total_bytes: Some(text.len() as u64),
        })
    }
    #[test]
    fn legacy_tool_bit_without_known_support_never_grants_tools() {
        for supported in [None, Some(0), Some(1)] {
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
                    capabilities: Some(3),
                    supported_capabilities: supported,
                    capability_source: supported.map(|_| 1),
                    ..Default::default()
                })),
                ..Default::default()
            };
            owner.receive(hello, &mut Vec::new()).unwrap();
            assert_eq!(owner.tools_available, supported == Some(1));
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
                    .begin([4; 16], 1, input, admitted, Instant::now())
                    .unwrap();
                assert_eq!(owner.tools_enabled, admitted && available);
                assert_eq!(owner.phase, Phase::Input);
            }
        }
    }

    #[test]
    fn missing_native_tools_does_not_hide_unready_or_expired_provider() {
        for phase in [Phase::Preparing, Phase::Draining] {
            let mut owner = owner(phase);
            owner.tools_available = false;
            assert!(
                owner
                    .begin([4; 16], 1, vec![], true, Instant::now())
                    .is_err()
            );
            assert!(owner.outgoing.is_empty());
        }
        let mut owner = owner(Phase::Available);
        owner.deadline = Instant::now();
        assert!(
            owner
                .begin([4; 16], 1, vec![], true, Instant::now())
                .is_err()
        );
        assert!(owner.outgoing.is_empty());
    }

    #[test]
    fn running_generation_does_not_expire_with_elapsed_time() {
        let mut owner = owner(Phase::Running);
        let now = Instant::now();
        owner.deadline = now - Duration::from_secs(120);
        assert_eq!(owner.next_deadline(now), None);
        assert!(owner.poll(now).is_empty());
        assert_eq!(owner.phase, Phase::Running);
        owner.cancel(1, now);
        assert_eq!(owner.phase, Phase::Draining);
        assert_eq!(owner.deadline, now + Duration::from_millis(250));
        assert_eq!(
            owner.next_deadline(now),
            None,
            "no child remains in this unit fixture"
        );
    }

    #[test]
    fn preparation_remains_bounded_and_cancellable() {
        let mut owner = owner(Phase::Preparing);
        let now = Instant::now();
        owner.deadline = now + Duration::from_secs(20);
        assert!(owner.poll(now + Duration::from_secs(19)).is_empty());
        assert!(matches!(
            owner.poll(now + Duration::from_secs(20)).first(),
            Some(ModelEvent::Failed("model_timeout"))
        ));
        assert!(owner.cancel_preparation.load(Ordering::Acquire));
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
        }
        assert!(
            matches!(&events[1],ModelEvent::Snapshot{revision:2,text} if text=="a complete response")
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
            .begin([4; 16], 1, input, false, Instant::now())
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
        owner.deadline = Instant::now() + Duration::from_millis(100);
        let ready_received = Instant::now();
        owner
            .receive(message(Body::Ready(pb::Ready {})), &mut events)
            .unwrap();
        assert!(owner.deadline >= ready_received + Duration::from_secs(5));
        owner.start().unwrap();
        assert!(owner.start().is_err());
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
    fn stale_start_and_cancel_reservation_are_bounded() {
        let mut owner = owner(Phase::Ready);
        owner.deadline = Instant::now();
        assert!(owner.start().is_err());
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
                reasoning_disabled: None,
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
                .begin([7; 16], 1, vec![], false, Instant::now())
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
                reasoning_disabled: None,
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
                capabilities: Some(1),
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
                    arguments: Some(pb::tool_call::Arguments::ListDirectory(
                        pb::ProjectListDirectory {
                            path: Some(".".into()),
                        },
                    )),
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
        assert!(matches!(&events[0], ModelEvent::Snapshot { text, .. } if text == "text"));
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
        for revision in 1..=5 {
            for body in [chunk("x", revision, 0), end("x", revision, 1)] {
                owner.incoming.extend(
                    model::encode_frame(&message(body), Direction::HelperToService).unwrap(),
                );
            }
        }
        owner.incoming.extend(
            model::encode_frame(
                &message(Body::Terminal(pb::Terminal {
                    outcome: Some(1),
                    last_revision: Some(5),
                    count: Some(5),
                    total_bytes: Some(5),
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
        assert_eq!(events.len(), 4);
        let now = Instant::now();
        assert_eq!(owner.next_deadline(now), Some(now));
        events.clear();
        owner.consume_input(&mut events).unwrap();
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
        owner.deadline = now - Duration::from_secs(1);
        assert_eq!(owner.next_deadline(now), owner.cleanup_retry);
    }
}
