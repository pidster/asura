//! The sole service lifecycle reactor and shared command pipeline.
mod audit;
mod completion;
mod config_worker;
mod context_manager;
mod context_observer;
mod conversation;
mod shell_worker;
// Real service fixtures share the canonical process-wide writer owner.
#[cfg(test)]
pub(crate) static TEST_WRITER: std::sync::Mutex<()> = std::sync::Mutex::new(());
mod guard_worker;
mod model_inventory;
mod model_owner;
mod sensors;
mod service_observer;
include!(concat!(env!("OUT_DIR"), "/model-package.rs"));
mod installation;
pub mod model;
pub mod tools;
use asura_control::{
    ControlCodec, Direction, Frame, ProtocolError, encode_frame, pb, validate_semantics,
    version_rejection,
};
use asura_platform::{
    AuthenticatedStream, PollInterest, RuntimeDirectory, ServiceLifetime, StartupNotice,
    StartupPipe,
};
use std::{
    io::{self, Read, Write},
    os::fd::AsRawFd,
    time::{Duration, Instant},
};

const CLIENT_LIMIT: usize = 32;
const FRAME_TIMEOUT: Duration = Duration::from_secs(2);
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const DRAIN_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug)]
pub enum Error {
    Platform(asura_platform::Error),
    Io(io::Error),
    Protocol(ProtocolError),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("service_unavailable")
    }
}
impl std::error::Error for Error {}
impl From<asura_platform::Error> for Error {
    fn from(v: asura_platform::Error) -> Self {
        Self::Platform(v)
    }
}
impl From<io::Error> for Error {
    fn from(v: io::Error) -> Self {
        Self::Io(v)
    }
}
impl From<ProtocolError> for Error {
    fn from(v: ProtocolError) -> Self {
        Self::Protocol(v)
    }
}

struct Connection {
    stream: AuthenticatedStream,
    codec: ControlCodec,
    attachment: Option<[u8; 16]>,
    counter: u64,
    output: Vec<u8>,
    written: usize,
    output_started: Option<Instant>,
    frame_started: Option<Instant>,
    last_request: Instant,
    close_after_write: bool,
    request_pending: Option<pb::Envelope>,
    validation: Option<(u64, pb::Envelope)>,
    validated: bool,
}
impl Connection {
    fn new(stream: AuthenticatedStream, now: Instant) -> Self {
        Self {
            stream,
            codec: ControlCodec::new(),
            attachment: None,
            counter: 0,
            output: Vec::new(),
            written: 0,
            output_started: None,
            frame_started: Some(now),
            last_request: now,
            close_after_write: false,
            request_pending: None,
            validation: None,
            validated: false,
        }
    }
    fn queue(&mut self, envelope: &pb::Envelope, now: Instant) -> Result<(), Error> {
        self.output = encode_frame(envelope)?;
        self.written = 0;
        self.output_started = Some(now);
        Ok(())
    }
    fn next_deadline(&self) -> Instant {
        [
            Some(self.last_request + IDLE_TIMEOUT),
            self.frame_started.map(|at| at + FRAME_TIMEOUT),
            self.output_started.map(|at| at + FRAME_TIMEOUT),
        ]
        .into_iter()
        .flatten()
        .min()
        .unwrap()
    }
    fn expired(&self, now: Instant) -> bool {
        self.frame_started
            .is_some_and(|at| now.duration_since(at) >= FRAME_TIMEOUT)
            || self
                .output_started
                .is_some_and(|at| now.duration_since(at) >= FRAME_TIMEOUT)
            || now.duration_since(self.last_request) >= IDLE_TIMEOUT
    }
    fn flush(&mut self) -> io::Result<bool> {
        if self.output.is_empty() {
            return Ok(!self.close_after_write);
        }
        match self.stream.stream_mut().write(&self.output[self.written..]) {
            Ok(0) => return Ok(false),
            Ok(n) => self.written += n,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                return Ok(true);
            }
            Err(e) => return Err(e),
        }
        if self.written == self.output.len() {
            self.output.clear();
            self.written = 0;
            self.output_started = None;
            return Ok(!self.close_after_write);
        }
        Ok(true)
    }
    fn receive(&mut self, now: Instant) -> Result<Option<Frame>, ProtocolError> {
        let mut bytes = [0; 8192];
        match self.stream.stream_mut().read(&mut bytes) {
            Ok(0) => Err(ProtocolError::MalformedWire),
            Ok(n) => {
                // A second request cannot be admitted while a reply is queued.
                if !self.output.is_empty() || self.request_pending.is_some() {
                    return Err(ProtocolError::InvalidSemantics);
                }
                self.frame_started.get_or_insert(now);
                let used = self.codec.push(&bytes[..n])?;
                if used != n {
                    return Err(ProtocolError::InvalidSemantics);
                }
                let result = self.codec.next_frame()?;
                if result.is_some() {
                    self.frame_started = None;
                }
                Ok(result)
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                Ok(None)
            }
            Err(_) => Err(ProtocolError::MalformedWire),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Service,
    Keep,
    Close,
    Drain,
    Config,
    Models,
    Conversation,
    Context,
}
struct Dispatcher {
    epoch: [u8; 16],
    build: String,
    installation: pb::InspectInstallationReply,
}
impl Dispatcher {
    fn status(&self, lifecycle: pb::Lifecycle) -> pb::InspectReply {
        pb::InspectReply {
            lifecycle: Some(lifecycle as i32),
            installation: self.installation.installation,
            reason: self.installation.reason,
            unavailable_reason: None,
        }
    }
    fn new(build: &str) -> Result<Self, Error> {
        let envelope = pb::Envelope {
            service_epoch: None,
            attachment_id: None,
            request_counter: None,
            body: Some(pb::envelope::Body::Hello(pb::Hello {
                client_build: Some(build.into()),
            })),
        };
        validate_semantics(&envelope, Direction::ClientToServer)?;
        Ok(Self {
            epoch: asura_platform::random_id(),
            build: build.into(),
            installation: installation::snapshot(
                pb::InstallationState::Recovering,
                pb::InstallationInspectionReason::InspectionPending,
            ),
        })
    }
    fn error(
        &self,
        connection: &mut Connection,
        request: &pb::Envelope,
        code: pb::ErrorCode,
        close: bool,
        now: Instant,
    ) -> Result<Action, Error> {
        let message = match code {
            pb::ErrorCode::InvalidSequence => "invalid_sequence",
            pb::ErrorCode::StaleOwner => "stale_owner",
            _ => "invalid_request",
        };
        let reply = pb::Envelope {
            service_epoch: request.service_epoch.clone(),
            attachment_id: request.attachment_id.clone(),
            request_counter: request.request_counter,
            body: Some(pb::envelope::Body::Error(pb::Error {
                code: Some(code as i32),
                message: Some(message.into()),
            })),
        };
        connection.queue(&reply, now)?;
        connection.close_after_write = close;
        Ok(Action::Keep)
    }
    fn dispatch(
        &self,
        connection: &mut Connection,
        request: pb::Envelope,
        now: Instant,
        lifecycle: pb::Lifecycle,
    ) -> Result<Action, Error> {
        use pb::envelope::Body;
        if connection.attachment.is_none() {
            if validate_semantics(&request, Direction::ClientToServer).is_err()
                || !matches!(request.body, Some(Body::Hello(_)))
            {
                return Ok(Action::Close);
            }
            let attachment = asura_platform::random_id();
            connection.attachment = Some(attachment);
            let reply = pb::Envelope {
                service_epoch: Some(self.epoch.to_vec()),
                attachment_id: Some(attachment.to_vec()),
                request_counter: None,
                body: Some(Body::HelloReply(pb::HelloReply {
                    service_build: Some(self.build.clone()),
                    capabilities: vec![
                        pb::Capability::Inspect as i32,
                        pb::Capability::Stop as i32,
                        pb::Capability::InspectInstallation as i32,
                        pb::Capability::Config as i32,
                        pb::Capability::Conversation as i32,
                        pb::Capability::Setup as i32,
                    ],
                    max_frame_bytes: Some(asura_control::MAX_FRAME_BYTES as u32),
                })),
            };
            connection.queue(&reply, now)?;
            connection.last_request = now;
            return Ok(Action::Keep);
        }
        let attachment = connection.attachment.unwrap();
        let Some(counter) = request.request_counter.filter(|n| *n != 0) else {
            return Ok(Action::Close);
        };
        if request.attachment_id.as_deref() != Some(attachment.as_slice()) {
            return Ok(Action::Close);
        }
        // The stale identity response must be safe and correlatable.
        if request
            .service_epoch
            .as_ref()
            .is_none_or(|id| id.len() != 16 || id.iter().all(|v| *v == 0))
        {
            return Ok(Action::Close);
        }
        if request.service_epoch.as_deref() != Some(self.epoch.as_slice()) {
            return self.error(connection, &request, pb::ErrorCode::StaleOwner, true, now);
        }
        if !valid_counter(connection.counter, counter) {
            return self.error(
                connection,
                &request,
                pb::ErrorCode::InvalidSequence,
                true,
                now,
            );
        }
        connection.counter = counter;
        connection.last_request = now;
        if let Some(Body::Stop(stop)) = &request.body
            && stop.expected_epoch.as_ref().is_some_and(|expected| {
                expected.len() == 16
                    && expected.iter().any(|byte| *byte != 0)
                    && expected.as_slice() != self.epoch.as_slice()
            })
        {
            return self.error(connection, &request, pb::ErrorCode::StaleOwner, true, now);
        }
        if validate_semantics(&request, Direction::ClientToServer).is_err() {
            return self.error(
                connection,
                &request,
                pb::ErrorCode::InvalidRequest,
                false,
                now,
            );
        }
        if matches!(request.body, Some(Body::ModelsList(_))) {
            if lifecycle != pb::Lifecycle::Serving {
                return self.error(connection, &request, pb::ErrorCode::Draining, false, now);
            }
            connection.request_pending = Some(request);
            return Ok(Action::Models);
        }
        if matches!(request.body, Some(Body::ConfigGet(_) | Body::ConfigSet(_))) {
            if lifecycle != pb::Lifecycle::Serving {
                return self.error(connection, &request, pb::ErrorCode::Draining, false, now);
            }
            connection.request_pending = Some(request);
            return Ok(Action::Config);
        }
        if matches!(request.body, Some(Body::ObserveContext(_))) {
            if lifecycle != pb::Lifecycle::Serving {
                return self.error(connection, &request, pb::ErrorCode::Draining, false, now);
            }
            connection.request_pending = Some(request);
            return Ok(Action::Context);
        }
        if matches!(request.body, Some(Body::ObserveService(_))) {
            connection.request_pending = Some(request);
            return Ok(Action::Service);
        }
        if conversation::handles(&request) {
            if lifecycle != pb::Lifecycle::Serving {
                return self.error(connection, &request, pb::ErrorCode::Draining, false, now);
            }
            connection.request_pending = Some(request);
            return Ok(Action::Conversation);
        }
        let (body, action) = match request.body.as_ref() {
            Some(Body::Inspect(_)) => (
                Body::InspectReply(pb::InspectReply {
                    lifecycle: Some(lifecycle as i32),
                    installation: self.installation.installation,
                    unavailable_reason: None,
                    reason: self.installation.reason,
                }),
                Action::Keep,
            ),
            Some(Body::InspectInstallation(_)) => (
                Body::InspectInstallationReply(self.installation.clone()),
                Action::Keep,
            ),
            Some(Body::Stop(_)) => (Body::StopAccepted(pb::StopAccepted {}), Action::Drain),
            _ => {
                return self.error(
                    connection,
                    &request,
                    pb::ErrorCode::InvalidRequest,
                    false,
                    now,
                );
            }
        };
        let reply = pb::Envelope {
            service_epoch: request.service_epoch,
            attachment_id: request.attachment_id,
            request_counter: request.request_counter,
            body: Some(body),
        };
        connection.queue(&reply, now)?;
        Ok(action)
    }
}
fn valid_counter(previous: u64, next: u64) -> bool {
    if previous == 0 {
        next == 1
    } else {
        next > previous
    }
}

/// Run the authenticated local service until Stop or a termination signal.
/// Runtime identity is checked at least every 100 ms and before each dispatch.
pub fn run(
    runtime: RuntimeDirectory,
    build: &str,
    notice: Option<StartupPipe>,
) -> Result<(), Error> {
    run_reactor(runtime, build, notice, None, installation::Worker::start)
}

/// Run a service whose launching TUI retains the lifetime pipe writer.
/// Writer closure uses the existing drain and never affects another service owner.
pub fn run_owned(
    runtime: RuntimeDirectory,
    build: &str,
    notice: Option<StartupPipe>,
    lifetime: ServiceLifetime,
) -> Result<(), Error> {
    run_reactor(
        runtime,
        build,
        notice,
        Some(lifetime),
        installation::Worker::start,
    )
}

#[cfg(test)]
fn run_with_worker<F>(
    runtime: RuntimeDirectory,
    build: &str,
    notice: Option<StartupPipe>,
    start_worker: F,
) -> Result<(), Error>
where
    F: FnOnce(RuntimeDirectory, [u8; 16]) -> io::Result<installation::Worker>,
{
    run_reactor(runtime, build, notice, None, start_worker)
}

fn run_reactor<F>(
    runtime: RuntimeDirectory,
    build: &str,
    notice: Option<StartupPipe>,
    lifetime: Option<ServiceLifetime>,
    start_worker: F,
) -> Result<(), Error>
where
    F: FnOnce(RuntimeDirectory, [u8; 16]) -> io::Result<installation::Worker>,
{
    run_reactor_with_config(
        runtime,
        build,
        notice,
        lifetime,
        start_worker,
        config_worker::Worker::start,
    )
}

fn run_reactor_with_config<F, G>(
    runtime: RuntimeDirectory,
    build: &str,
    mut notice: Option<StartupPipe>,
    mut lifetime: Option<ServiceLifetime>,
    start_worker: F,
    mut start_config: G,
) -> Result<(), Error>
where
    F: FnOnce(RuntimeDirectory, [u8; 16]) -> io::Result<installation::Worker>,
    G: FnMut(RuntimeDirectory, pb::Envelope, Instant) -> io::Result<config_worker::Worker>,
{
    let mut dispatcher = Dispatcher::new(build)?;
    let mut guard = match runtime.acquire_owner() {
        Ok(guard) => guard,
        Err(error) => {
            if let Some(pipe) = notice.take() {
                let kind = match &error {
                    asura_platform::Error::OwnerBusy => StartupNotice::OwnerBusy,
                    asura_platform::Error::UnsafeRuntime => StartupNotice::UnsafeRuntime,
                    _ => StartupNotice::Unavailable,
                };
                let _ = pipe.send(kind);
                if matches!(error, asura_platform::Error::OwnerBusy) {
                    // A private spawn that loses arbitration has no work to do.
                    // The launching client attaches to the existing owner.
                    return Ok(());
                }
            }
            return Err(error.into());
        }
    };
    let listener = match guard.bind() {
        Ok(listener) => listener,
        Err(error) => {
            if let Some(pipe) = notice.take() {
                let _ = pipe.send(StartupNotice::Unavailable);
            }
            return Err(error.into());
        }
    };
    let mut guard = Some(guard);
    let mut guard_job: Option<guard_worker::Worker> = None;
    let mut guard_slot: Option<(usize, u64)> = None;
    let mut validation_ticket = 0u64;
    let mut validation_schedule = 0usize;
    asura_platform::install_signals()?;
    let (wake_reader, wake_sender) = asura_platform::events::wake_pair()?;
    let mut inspection =
        installation::Deferred::new(runtime.clone(), dispatcher.epoch, start_worker);
    inspection.register(wake_sender.clone());
    if let Some(pipe) = notice.take() {
        let _ = pipe.send(StartupNotice::Bound);
    }
    tracing::info!("service_serving");
    let mut clients: Vec<Option<Connection>> = (0..CLIENT_LIMIT).map(|_| None).collect();
    let mut config_job: Option<config_worker::Worker> = None;
    let mut models =
        model_inventory::Owner::new(runtime.clone(), MODEL_PACKAGE, wake_sender.clone());
    let mut conversations = conversation::Owner::new(runtime.clone(), MODEL_PACKAGE);
    let mut audit = audit::Worker::start(
        runtime.clone(),
        dispatcher.epoch,
        build.into(),
        wake_sender.clone(),
    )
    .ok();
    let mut audit_shutdown_reason = None;
    if let Some(worker) = &audit {
        conversations.set_audit_emitter(worker.emitter());
        let snapshot = worker.snapshot(Instant::now());
        conversations.set_audit_snapshot(snapshot.records);
        conversations.set_audit_health(snapshot.health);
    }
    let mut contexts = context_manager::Manager::default();
    contexts.register(wake_sender.clone());
    conversations.register(wake_sender.clone());
    let mut drain: Option<(usize, Instant)> = None;
    let mut lifecycle = pb::Lifecycle::Serving;
    let mut service_observer = service_observer::Observer::new(
        runtime.clone(),
        wake_sender.clone(),
        dispatcher.status(lifecycle),
    );
    let mut endpoint_removed = false;
    let mut lifetime_ended = false;
    let mut retry_after_inspection = false;
    let result = (|| -> Result<(), Error> {
        loop {
            wake_reader.drain()?;
            let now = Instant::now();
            let attachments: Vec<_> = clients
                .iter()
                .flatten()
                .filter_map(|client| client.attachment)
                .collect();
            conversations.retain_observers(&attachments);
            if let Some(reply) = models.poll(&attachments, now) {
                for client in clients.iter_mut().flatten() {
                    if client.request_pending.as_ref().is_some_and(|request| {
                        request.attachment_id == reply.attachment_id
                            && request.request_counter == reply.request_counter
                    }) {
                        client.request_pending = None;
                        client.queue(&reply, now)?;
                    }
                }
            }
            for reply in contexts
                .poll(&attachments, now)
                .into_iter()
                .chain(service_observer.poll(dispatcher.status(lifecycle), &attachments, now))
            {
                for client in clients.iter_mut().flatten() {
                    if client.request_pending.as_ref().is_some_and(|request| {
                        request.attachment_id == reply.attachment_id
                            && request.request_counter == reply.request_counter
                    }) {
                        client.request_pending = None;
                        client.queue(&reply, now)?;
                    }
                }
            }
            if let Some(event) = guard_job.as_mut().and_then(|job| job.poll(now)) {
                match event {
                    guard_worker::Event::Deadline { ticket } => {
                        if let Some((slot, expected)) = guard_slot.take()
                            && ticket == expected
                            && clients[slot].as_ref().is_some_and(|client| {
                                client
                                    .validation
                                    .as_ref()
                                    .is_some_and(|(current, _)| *current == ticket)
                            })
                        {
                            clients[slot] = None;
                        }
                    }
                    guard_worker::Event::Finished {
                        ticket,
                        result,
                        guard: returned,
                        expired,
                    } => {
                        if let Some(returned) = returned {
                            guard = Some(returned);
                            result?;
                            endpoint_removed = true;
                            guard_job = None;
                            break;
                        }
                        if guard.is_none() {
                            return Err(asura_platform::Error::Unavailable.into());
                        }
                        if let Some((slot, expected)) = guard_slot.take()
                            && expected == ticket
                            && !expired
                        {
                            result?;
                            if let Some(client) = clients[slot].as_mut().filter(|client| {
                                client
                                    .validation
                                    .as_ref()
                                    .is_some_and(|(id, _)| *id == ticket)
                            }) {
                                client.validated = true;
                            }
                        }
                        guard_job = None;
                    }
                }
            }
            if audit
                .as_ref()
                .is_none_or(|worker| worker.bootstrap_settled())
            {
                inspection.release()?;
            }
            if let Some(snapshot) = inspection.poll(dispatcher.epoch, now) {
                dispatcher.installation = snapshot;
                wake_sender.notify()?;
            }
            if let Some(job) = config_job.as_mut() {
                if let Some(reply) = job.poll(now) {
                    service_observer.refresh();
                    for client in clients.iter_mut().flatten() {
                        if client.request_pending.as_ref().is_some_and(|request| {
                            request.attachment_id == reply.attachment_id
                                && request.request_counter == reply.request_counter
                        }) {
                            client.request_pending = None;
                            client.queue(&reply, now)?;
                        }
                    }
                }
                if let Some(event) = job.take_audit_event()
                    && let Some(worker) = &audit
                {
                    let _ = worker.emitter().try_emit(event);
                }
                if job.settled() {
                    config_job = None;
                }
            }
            if let Some(worker) = &mut audit {
                if let Some(snapshot) = worker.poll(now) {
                    conversations.set_audit_snapshot(snapshot.records);
                }
                conversations.set_audit_health(worker.health(now));
                if let Some(reason) = audit_shutdown_reason
                    && config_job.is_none()
                    && conversations.settled()
                {
                    worker.close_with_reason(now, reason);
                }
            }
            if lifecycle == pb::Lifecycle::Serving && inspection.settled() {
                conversations.open();
            }
            conversations.sensor_status(dispatcher.epoch, dispatcher.status(lifecycle));
            for reply in conversations.poll(now) {
                for client in clients.iter_mut().flatten() {
                    if client.request_pending.as_ref().is_some_and(|request| {
                        request.attachment_id == reply.attachment_id
                            && request.request_counter == reply.request_counter
                    }) {
                        client.request_pending = None;
                        client.queue(&reply, now)?;
                    }
                }
            }
            if let Some(snapshot) = conversations.installation()
                && dispatcher.installation != snapshot
            {
                dispatcher.installation = snapshot;
                wake_sender.notify()?;
            }
            let owner_ended = !lifetime_ended
                && lifetime
                    .as_mut()
                    .is_some_and(|reader| reader.ended().unwrap_or(true));
            lifetime_ended |= owner_ended;
            let signal = asura_platform::take_termination_signal().is_some();
            let settled_retry = retry_after_inspection
                && inspection.settled()
                && config_job.is_none()
                && models.settled()
                && conversations.settled()
                && contexts.settled()
                && service_observer.settled()
                && audit.as_ref().is_none_or(|worker| worker.settled());
            if owner_ended || signal || settled_retry {
                for client in &mut clients {
                    *client = None;
                }
                let reason = if signal {
                    "signal"
                } else {
                    "owner_lifetime_ended"
                };
                audit_shutdown_reason.get_or_insert(if signal {
                    asura_storage::audit::StopReason::Signal
                } else {
                    asura_storage::audit::StopReason::OwnerLifetimeEnded
                });
                tracing::info!(reason, "service_draining");
                inspection.cancel();
                conversations.shutdown();
                contexts.shutdown();
                service_observer.shutdown();
                models.shutdown(now);
                if let Some(job) = &config_job {
                    job.cancel();
                }
                // Repeated EOF or signals must not extend an existing drain budget.
                drain.get_or_insert((CLIENT_LIMIT, now + DRAIN_TIMEOUT));
                retry_after_inspection = false;
            }
            if let Some((slot, deadline)) = drain
                && (now >= deadline || slot == CLIENT_LIMIT || clients[slot].is_none())
            {
                for client in &mut clients {
                    *client = None;
                }
                if !inspection.settled()
                    || config_job.is_some()
                    || !models.settled()
                    || !conversations.settled()
                    || !contexts.settled()
                    || !service_observer.settled()
                    || audit.as_ref().is_some_and(|worker| !worker.settled())
                    || guard_job.as_ref().is_some_and(|job| !job.settled())
                {
                    if now >= deadline
                        || audit
                            .as_ref()
                            .is_some_and(|worker| worker.settlement_expired(now))
                    {
                        lifecycle = pb::Lifecycle::RepairOnly;
                        dispatcher.installation = installation::snapshot(
                            pb::InstallationState::Unavailable,
                            pb::InstallationInspectionReason::InspectionTimeout,
                        );
                        drain = None;
                        // Keep the lock and repair endpoint while the worker settles.
                        // A TUI-owned service must then finish its requested shutdown.
                        retry_after_inspection = true;
                    }
                } else {
                    if let Some(owned) = guard.take() {
                        match guard_worker::Worker::start_cleanup(owned, 0, now) {
                            Ok(job) => {
                                job.register(wake_sender.clone());
                                guard_job = Some(job);
                            }
                            Err((error, returned)) => {
                                guard = Some(returned);
                                return Err(error.into());
                            }
                        }
                    }
                }
            }
            for client in &mut clients {
                if client.as_ref().is_some_and(|c| c.expired(now)) {
                    *client = None;
                }
            }
            if guard_job.is_none()
                && drain.is_none()
                && let Some(guard) = guard.as_ref()
            {
                let candidates: Vec<_> = clients
                    .iter()
                    .enumerate()
                    .filter_map(|(slot, client)| {
                        let client = client.as_ref()?;
                        if client.validated {
                            return None;
                        }
                        let (ticket, request) = client.validation.as_ref()?;
                        let priority = match request.body {
                            Some(
                                pb::envelope::Body::Stop(_)
                                | pb::envelope::Body::ConversationCancel(_),
                            ) => 0,
                            Some(
                                pb::envelope::Body::ObserveService(_)
                                | pb::envelope::Body::ConversationObserve(_)
                                | pb::envelope::Body::Inspect(_)
                                | pb::envelope::Body::InspectInstallation(_)
                                | pb::envelope::Body::ObserveContext(_)
                                | pb::envelope::Body::ConversationQueueList(_),
                            ) => 2,
                            _ => 1,
                        };
                        Some((priority, *ticket, slot))
                    })
                    .collect();
                let mut candidate = None;
                for _ in 0..17 {
                    let priority = match validation_schedule {
                        0..=7 => 0,
                        8..=15 => 1,
                        _ => 2,
                    };
                    validation_schedule = (validation_schedule + 1) % 17;
                    candidate = candidates
                        .iter()
                        .filter(|(class, _, _)| *class == priority)
                        .min()
                        .copied();
                    if candidate.is_some() {
                        break;
                    }
                }
                if let Some((_, ticket, slot)) = candidate {
                    let job =
                        guard_worker::Worker::start(guard.validation_snapshot(), ticket, now)?;
                    job.register(wake_sender.clone());
                    guard_slot = Some((slot, ticket));
                    guard_job = Some(job);
                }
            }
            let mut interests = vec![PollInterest {
                fd: listener.as_raw_fd(),
                read: drain.is_none() && guard.is_some(),
                write: false,
            }];
            let mut slots = vec![None];
            for (slot, client) in clients.iter().enumerate() {
                if let Some(client) = client {
                    interests.push(PollInterest {
                        fd: client.stream.stream().as_raw_fd(),
                        read: drain.is_none()
                            && !client.close_after_write
                            && client.validation.is_none(),
                        write: !client.output.is_empty(),
                    });
                    slots.push(Some(slot));
                }
            }
            for interest in [
                PollInterest {
                    fd: wake_reader.as_raw_fd(),
                    read: true,
                    write: false,
                },
                PollInterest {
                    fd: asura_platform::termination_descriptor()?,
                    read: true,
                    write: false,
                },
            ]
            .into_iter()
            .chain(
                lifetime
                    .as_ref()
                    .filter(|_| !lifetime_ended)
                    .map(|life| PollInterest {
                        fd: life.as_raw_fd(),
                        read: true,
                        write: false,
                    }),
            )
            .chain(conversations.interests())
            .chain(models.interests())
            {
                interests.push(interest);
                slots.push(None);
            }
            let deadline = clients
                .iter()
                .flatten()
                .map(Connection::next_deadline)
                .chain(inspection.next_deadline(now))
                .chain(config_job.as_ref().and_then(|job| job.next_deadline(now)))
                .chain(contexts.next_deadline(now))
                .chain(service_observer.next_deadline(now))
                .chain(audit.as_ref().and_then(|worker| worker.next_deadline(now)))
                .chain(conversations.next_deadline(now))
                .chain(models.next_deadline(now))
                .chain(guard_job.as_ref().and_then(|job| job.next_deadline(now)))
                .chain(
                    drain
                        .map(|(_, deadline)| deadline)
                        .filter(|deadline| *deadline > now),
                )
                .min();
            let has_validated = clients.iter().flatten().any(|client| client.validated);
            let timeout = deadline
                .map(|at| at.saturating_duration_since(Instant::now()))
                .unwrap_or(Duration::from_secs(86400));
            let mut ready = asura_platform::poll(
                &interests,
                if has_validated {
                    Duration::ZERO
                } else {
                    timeout
                },
            )?;
            for (index, slot) in slots.iter().enumerate() {
                if slot.is_some_and(|slot| {
                    clients[slot]
                        .as_ref()
                        .is_some_and(|client| client.validated)
                }) {
                    ready.push(asura_platform::Ready {
                        index,
                        read: false,
                        write: false,
                        closed: false,
                    });
                }
            }
            for event in ready {
                if event.index == 0 {
                    if drain.is_some() {
                        continue;
                    }
                    for _ in 0..CLIENT_LIMIT {
                        match listener.accept() {
                            Ok((stream, _)) => {
                                let Some(slot) = clients.iter().position(Option::is_none) else {
                                    drop(stream);
                                    continue;
                                };
                                if let Ok(stream) = AuthenticatedStream::from_stream(stream) {
                                    clients[slot] = Some(Connection::new(stream, Instant::now()));
                                }
                            }
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                            Err(e) => return Err(e.into()),
                        }
                    }
                    continue;
                }
                let Some(slot) = slots[event.index] else {
                    continue;
                };
                let Some(client) = clients[slot].as_mut() else {
                    continue;
                };
                let now = Instant::now();
                let mut action = Action::Keep;
                if client.validated && drain.is_none() {
                    client.validated = false;
                    let (_, request) = client
                        .validation
                        .take()
                        .expect("validated request retained");
                    action = dispatcher.dispatch(client, request, now, lifecycle)?;
                } else if event.read
                    && drain.is_none()
                    && !client.close_after_write
                    && client.validation.is_none()
                {
                    match client.receive(now) {
                        Ok(Some(Frame::Message(request))) => {
                            validation_ticket = validation_ticket
                                .checked_add(1)
                                .ok_or(asura_platform::Error::Unavailable)?;
                            client.validation = Some((validation_ticket, *request));
                            wake_sender.notify()?;
                        }
                        Ok(Some(Frame::VersionRejected)) => action = Action::Close,
                        Ok(None) => (),
                        Err(ProtocolError::UnsupportedVersion { .. })
                            if client.attachment.is_none() =>
                        {
                            client.output = version_rejection().to_vec();
                            client.written = 0;
                            client.output_started = Some(now);
                            client.close_after_write = true;
                        }
                        Err(_) => action = Action::Close,
                    }
                }
                if action != Action::Close && event.write && !client.flush().unwrap_or(false) {
                    action = Action::Close;
                }
                // A hangup with no readable data cannot yield another request.
                if event.closed && !event.read && client.output.is_empty() {
                    action = Action::Close;
                }
                match action {
                    Action::Service => {
                        let request = client.request_pending.as_ref().unwrap().clone();
                        if let Some(reply) = service_observer.request(request, now) {
                            client.request_pending = None;
                            client.queue(&reply, now)?;
                        }
                    }
                    Action::Close => clients[slot] = None,
                    Action::Keep => (),
                    Action::Config => {
                        let request = client.request_pending.as_ref().unwrap().clone();
                        let error = if config_job.is_some()
                            || (matches!(request.body, Some(pb::envelope::Body::ConfigSet(_)))
                                && !inspection.settled())
                        {
                            Some("config_busy")
                        } else {
                            match start_config(runtime.clone(), request.clone(), now) {
                                Ok(job) => {
                                    job.register(wake_sender.clone());
                                    config_job = Some(job);
                                    None
                                }
                                Err(_) => Some("config_unavailable"),
                            }
                        };
                        if let Some(error) = error {
                            client.request_pending = None;
                            let mut reply = request;
                            reply.body = Some(pb::envelope::Body::ConfigReply(pb::ConfigReply {
                                value_yaml: None,
                                error: Some(error.into()),
                            }));
                            client.queue(&reply, now)?;
                        }
                    }
                    Action::Models => {
                        let request = client.request_pending.as_ref().unwrap().clone();
                        if let Some(reply) = models.request(request, now) {
                            client.request_pending = None;
                            client.queue(&reply, now)?;
                        }
                    }
                    Action::Conversation => {
                        let request = client.request_pending.as_ref().unwrap().clone();
                        if let Some(reply) = conversations.request(request, now) {
                            client.request_pending = None;
                            client.queue(&reply, now)?;
                        }
                    }
                    Action::Context => {
                        let request = client.request_pending.as_ref().unwrap().clone();
                        let identity = if let Some(pb::envelope::Body::ObserveContext(query)) =
                            &request.body
                        {
                            query
                                .project_id
                                .as_deref()
                                .and_then(|id| id.try_into().ok())
                                .and_then(|id| conversations.project_identity(id))
                        } else {
                            None
                        };
                        if let Some(reply) = contexts.request(request, identity, now) {
                            client.request_pending = None;
                            client.queue(&reply, now)?;
                        }
                    }
                    Action::Drain => {
                        audit_shutdown_reason.get_or_insert(asura_storage::audit::StopReason::Stop);
                        tracing::info!(reason = "stop", "service_draining");
                        inspection.cancel();
                        conversations.shutdown();
                        contexts.shutdown();
                        service_observer.shutdown();
                        models.shutdown(now);
                        if let Some(job) = &config_job {
                            job.cancel();
                        }
                        drain = Some((slot, now + DRAIN_TIMEOUT));
                        for (index, entry) in clients.iter_mut().enumerate() {
                            if index == slot {
                                if let Some(client) = entry {
                                    client.close_after_write = true;
                                }
                            } else {
                                *entry = None;
                            }
                        }
                        break;
                    }
                }
            }
        }
        Ok(())
    })();
    // An unsafe runtime error forbids further dispatch, but cannot release ownership
    // while any worker is still using retained filesystem handles.
    audit_shutdown_reason.get_or_insert(asura_storage::audit::StopReason::OwnerLifetimeEnded);
    inspection.cancel();
    conversations.shutdown();
    contexts.shutdown();
    service_observer.shutdown();
    models.shutdown(Instant::now());
    if let Some(job) = &config_job {
        job.cancel();
    }
    if let Some(job) = &guard_job {
        job.cancel();
    }
    while !inspection.settled()
        || config_job.is_some()
        || !models.settled()
        || !conversations.settled()
        || !contexts.settled()
        || !service_observer.settled()
        || audit.as_ref().is_some_and(|worker| !worker.settled())
        || guard_job.is_some()
    {
        wake_reader.drain()?;
        let now = Instant::now();
        let _ = contexts.poll(&[], now);
        let _ = service_observer.poll(dispatcher.status(lifecycle), &[], now);
        let _ = conversations.poll(now);
        if let Some(worker) = &mut audit {
            let _ = worker.poll(now);
            if config_job.is_none() && conversations.settled() {
                worker.close_with_reason(now, audit_shutdown_reason.unwrap());
            }
        }
        let _ = models.poll(&[], now);
        let _ = inspection.poll(dispatcher.epoch, now);
        if let Some(job) = config_job.as_mut() {
            let _ = job.poll(now);
            if let Some(event) = job.take_audit_event()
                && let Some(worker) = &audit
            {
                let _ = worker.emitter().try_emit(event);
            }
            if job.settled() {
                config_job = None;
            }
        }
        if let Some(guard_worker::Event::Finished {
            guard: returned,
            result,
            ..
        }) = guard_job.as_mut().and_then(|job| job.poll(now))
        {
            if let Some(returned) = returned {
                guard = Some(returned);
                endpoint_removed = result.is_ok();
            }
            guard_job = None;
        }
        if !inspection.settled()
            || config_job.is_some()
            || !models.settled()
            || !conversations.settled()
            || !contexts.settled()
            || !service_observer.settled()
            || audit.as_ref().is_some_and(|worker| !worker.settled())
            || guard_job.is_some()
        {
            let deadline = inspection
                .next_deadline(now)
                .into_iter()
                .chain(config_job.as_ref().and_then(|job| job.next_deadline(now)))
                .chain(contexts.next_deadline(now))
                .chain(service_observer.next_deadline(now))
                .chain(audit.as_ref().and_then(|worker| worker.next_deadline(now)))
                .chain(conversations.next_deadline(now))
                .chain(models.next_deadline(now))
                .chain(guard_job.as_ref().and_then(|job| job.next_deadline(now)))
                .min();
            let mut interests = conversations.interests();
            interests.extend(models.interests());
            interests.push(PollInterest {
                fd: wake_reader.as_raw_fd(),
                read: true,
                write: false,
            });
            asura_platform::poll(
                &interests,
                deadline
                    .map(|at| at.saturating_duration_since(Instant::now()))
                    .unwrap_or(Duration::from_secs(86400)),
            )?;
        }
    }
    drop(clients);
    drop(listener);
    let cleanup = if endpoint_removed {
        Ok(())
    } else if let Some(owned) = guard.take() {
        match guard_worker::Worker::start_cleanup(owned, 0, Instant::now()) {
            Err((error, returned)) => {
                guard = Some(returned);
                Err(error.into())
            }
            Ok(mut job) => {
                job.register(wake_sender.clone());
                loop {
                    wake_reader.drain()?;
                    let now = Instant::now();
                    if let Some(guard_worker::Event::Finished {
                        guard: returned,
                        result,
                        ..
                    }) = job.poll(now)
                    {
                        guard = returned;
                        break result;
                    }
                    asura_platform::poll(
                        &[PollInterest {
                            fd: wake_reader.as_raw_fd(),
                            read: true,
                            write: false,
                        }],
                        job.next_deadline(now)
                            .map(|at| at.saturating_duration_since(Instant::now()))
                            .unwrap_or(Duration::from_secs(86400)),
                    )?;
                }
            }
        }
    } else {
        Err(asura_platform::Error::Unavailable)
    };
    // Guard is deliberately held through endpoint settlement and dropped last.
    drop(guard);
    match (result, cleanup) {
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) => Err(error.into()),
        (Ok(()), Ok(())) => {
            tracing::info!("service_stopped");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn request_counters_start_at_one_and_never_replay() {
        assert!(valid_counter(0, 1));
        assert!(!valid_counter(0, 0));
        assert!(!valid_counter(0, 2));
        assert!(valid_counter(1, 2));
        assert!(valid_counter(1, 4));
        assert!(!valid_counter(2, 2));
        assert!(!valid_counter(2, 1));
        assert!(!valid_counter(u64::MAX, 1));
    }
    #[test]
    fn dispatcher_replay_and_stale_stop_never_enter_drain() {
        use pb::envelope::Body;
        let (stream, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut connection = Connection::new(
            AuthenticatedStream::from_stream(stream).unwrap(),
            Instant::now(),
        );
        let dispatcher = Dispatcher::new("asura/test").unwrap();
        let hello = pb::Envelope {
            service_epoch: None,
            attachment_id: None,
            request_counter: None,
            body: Some(Body::Hello(pb::Hello {
                client_build: Some("asura/test".into()),
            })),
        };
        assert_eq!(
            dispatcher
                .dispatch(
                    &mut connection,
                    hello,
                    Instant::now(),
                    pb::Lifecycle::Serving
                )
                .unwrap(),
            Action::Keep
        );
        let mut request = pb::Envelope {
            service_epoch: Some(dispatcher.epoch.to_vec()),
            attachment_id: Some(connection.attachment.unwrap().to_vec()),
            request_counter: Some(1),
            body: Some(Body::Inspect(pb::Inspect {})),
        };
        assert_eq!(
            dispatcher
                .dispatch(
                    &mut connection,
                    request.clone(),
                    Instant::now(),
                    pb::Lifecycle::RepairOnly
                )
                .unwrap(),
            Action::Keep
        );
        let reply =
            asura_control::decode_body(&connection.output[asura_control::HEADER_BYTES..]).unwrap();
        let Some(Body::InspectReply(status)) = reply.body else {
            panic!("missing inspection");
        };
        assert_eq!(status.lifecycle, Some(pb::Lifecycle::RepairOnly as i32));
        assert_eq!(
            status.reason,
            Some(pb::InstallationInspectionReason::InspectionPending as i32)
        );
        request.body = Some(Body::Stop(pb::Stop {
            expected_epoch: Some(dispatcher.epoch.to_vec()),
        }));
        assert_eq!(
            dispatcher
                .dispatch(
                    &mut connection,
                    request.clone(),
                    Instant::now(),
                    pb::Lifecycle::Serving
                )
                .unwrap(),
            Action::Keep
        );
        let reply =
            asura_control::decode_body(&connection.output[asura_control::HEADER_BYTES..]).unwrap();
        let Some(Body::Error(error)) = reply.body else {
            panic!("missing sequence error");
        };
        assert_eq!(error.code, Some(pb::ErrorCode::InvalidSequence as i32));
        request.request_counter = Some(2);
        request.body = Some(Body::Stop(pb::Stop {
            expected_epoch: Some(vec![7; 16]),
        }));
        assert_eq!(
            dispatcher
                .dispatch(
                    &mut connection,
                    request.clone(),
                    Instant::now(),
                    pb::Lifecycle::Serving
                )
                .unwrap(),
            Action::Keep
        );
        let reply =
            asura_control::decode_body(&connection.output[asura_control::HEADER_BYTES..]).unwrap();
        let Some(Body::Error(error)) = reply.body else {
            panic!("missing stale expected-epoch error");
        };
        assert_eq!(error.code, Some(pb::ErrorCode::StaleOwner as i32));
        request.request_counter = Some(3);
        request.service_epoch = Some(vec![7; 16]);
        request.body = Some(Body::Stop(pb::Stop {
            expected_epoch: Some(vec![7; 16]),
        }));
        assert_eq!(
            dispatcher
                .dispatch(
                    &mut connection,
                    request,
                    Instant::now(),
                    pb::Lifecycle::Serving
                )
                .unwrap(),
            Action::Keep
        );
        let reply =
            asura_control::decode_body(&connection.output[asura_control::HEADER_BYTES..]).unwrap();
        let Some(Body::Error(error)) = reply.body else {
            panic!("missing stale-owner error");
        };
        assert_eq!(error.code, Some(pb::ErrorCode::StaleOwner as i32));
    }
    #[test]
    fn service_build_label_uses_control_validation() {
        assert!(Dispatcher::new("asura/0.1.0").is_ok());
        assert!(Dispatcher::new("").is_err());
        assert!(Dispatcher::new("a\nb").is_err());
    }
}
