//! Bounded attachment and lifecycle requests over the local control socket.
use asura_control::{
    ControlCodec, Direction, Frame, encode_frame, pb, validate_identity, validate_semantics,
};
use asura_platform::{
    AuthenticatedStream, LockWitness, PollInterest, RuntimeDirectory, StartupNotice,
};
use std::{
    io::{Read, Write},
    os::fd::AsRawFd,
    time::Duration,
};
mod response;
pub use response::ResponseAssembly;

/// What ends a client wait besides the reply or a lost connection: the
/// person's cancel or exit (owner, 2026-10-16). It is checked at the pacing
/// cadence [`CANCEL_POLL`] and never ends a wait by itself.
pub type Cancel = std::sync::Arc<dyn Fn() -> bool + Send + Sync>;

#[derive(Debug)]
pub enum Error {
    Platform(asura_platform::Error),
    Protocol,
    Incompatible,
    Unavailable,
    OutcomeUnconfirmed,
    /// The person cancelled a read wait; nothing was changed by this client.
    Cancelled,
    Remote(i32, String),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Platform(asura_platform::Error::UnsafeRuntime) => "unsafe_runtime",
            Self::Incompatible => "incompatible_protocol",
            Self::OutcomeUnconfirmed => "outcome_unconfirmed",
            Self::Cancelled => "cancelled",
            Self::Remote(_, message) => message,
            _ => "service_unavailable",
        })
    }
}
impl std::error::Error for Error {}
impl From<asura_platform::Error> for Error {
    fn from(value: asura_platform::Error) -> Self {
        Self::Platform(value)
    }
}
pub type Result<T> = std::result::Result<T, Error>;

fn rename_reply_matches(project_id: &Option<Vec<u8>>, reply: &pb::ProjectRenameReply) -> bool {
    let matching_projects = || {
        reply
            .project
            .as_ref()
            .zip(reply.current_project.as_ref())
            .is_some_and(|(outcome, current)| {
                outcome.project_id.as_ref() == project_id.as_ref()
                    && current.project_id.as_ref() == project_id.as_ref()
                    && outcome.name.is_some()
                    && current.name.is_some()
                    && outcome.name_revision.is_some()
                    && current.name_revision.is_some()
                    && current.name_revision >= outcome.name_revision
            })
    };
    match reply.error.as_deref() {
        None => reply.changed.is_some() && matching_projects(),
        Some("stale_project_name_revision") => reply.changed.is_none() && matching_projects(),
        Some(_) => {
            reply.changed.is_none() && reply.project.is_none() && reply.current_project.is_none()
        }
    }
}

fn queue_submit_reply_matches(
    request: &pb::ConversationQueueSubmit,
    reply: &pb::ConversationQueueReply,
) -> bool {
    reply.request_id == request.request_id
        && reply.stale_order == Some(false)
        && reply.order_revision.is_some()
        && reply.full_text == Some(false)
        && reply.accepted_input_id.is_some()
        && reply
            .entries
            .iter()
            .all(|entry| entry.project_id == request.project_id)
        && reply.entries.iter().any(|entry| {
            entry.input_id == reply.accepted_input_id
                && request
                    .conversation_id
                    .as_ref()
                    .is_none_or(|id| entry.conversation_id.as_ref() == Some(id))
        })
}

fn queue_reorder_reply_matches(
    request: &pb::ConversationQueueReorder,
    reply: &pb::ConversationQueueReply,
) -> bool {
    reply.request_id == request.request_id
        && reply.stale_order.is_some()
        && reply.order_revision.is_some()
        && reply.full_text == Some(false)
        && reply.accepted_input_id.is_none()
}

pub struct ServiceUpdate {
    pub uptime_ms: u64,
    pub stored_memory: pb::StoredMemoryStatus,
    pub snapshot: Snapshot,
    pub revision: u64,
    pub pending: bool,
    pub configured_model: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub service_epoch: [u8; 16],
    pub service_build: String,
    pub lifecycle: i32,
    pub installation: i32,
    pub reason: i32,
    /// Automatic initialization completed in the current service run.
    pub new_installation: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallationSnapshot {
    pub service_epoch: [u8; 16],
    pub installation: pb::InstallationState,
    pub reason: pb::InstallationInspectionReason,
    pub installation_id: Option<[u8; 16]>,
    pub authority_revision: Option<u64>,
    pub recorded_owner_generation: Option<u64>,
    pub binding_generation: Option<u64>,
    pub authority_format: Option<u32>,
}

pub struct Client {
    runtime: RuntimeDirectory,
    witness: LockWitness,
    stream: AuthenticatedStream,
    codec: ControlCodec,
    epoch: [u8; 16],
    attachment: [u8; 16],
    counter: u64,
    build: String,
    /// Set when a cancelled or lost wait left a reply in flight; later requests refuse.
    abandoned: bool,
    /// Ends this attachment's waits besides a reply or lost connection.
    cancel: Cancel,
}

/// The cancel check cadence of a wait. Pacing only: it ends nothing by itself.
const CANCEL_POLL: Duration = Duration::from_millis(50);
/// The poll slice of one wait step; the person's cancel ends the wait.
fn slice(cancelled: &dyn Fn() -> bool) -> Result<Duration> {
    if cancelled() {
        Err(Error::Cancelled)
    } else {
        Ok(CANCEL_POLL)
    }
}
fn never() -> bool {
    false
}
fn wait_until(
    stream: &AuthenticatedStream,
    write: bool,
    cancelled: &dyn Fn() -> bool,
) -> Result<()> {
    let interest = PollInterest {
        fd: stream.stream().as_raw_fd(),
        read: !write,
        write,
    };
    asura_platform::poll(&[interest], slice(cancelled)?)?;
    Ok(())
}
fn write_frame_until(
    stream: &mut AuthenticatedStream,
    envelope: &pb::Envelope,
    cancelled: &dyn Fn() -> bool,
) -> Result<()> {
    let bytes = encode_frame(envelope).map_err(|_| Error::Protocol)?;
    let mut offset = 0;
    while offset < bytes.len() {
        slice(cancelled)?;
        match stream.stream_mut().write(&bytes[offset..]) {
            Ok(0) => return Err(Error::Unavailable),
            Ok(n) => offset += n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                wait_until(stream, true, cancelled)?
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => (),
            Err(_) => return Err(Error::Unavailable),
        }
    }
    Ok(())
}
fn read_frame_until(
    stream: &mut AuthenticatedStream,
    codec: &mut ControlCodec,
    cancelled: &dyn Fn() -> bool,
) -> Result<Frame> {
    let mut buffer = [0u8; 8192];
    loop {
        slice(cancelled)?;
        if let Some(frame) = codec.next_frame().map_err(|_| Error::Protocol)? {
            return Ok(frame);
        }
        match stream.stream_mut().read(&mut buffer) {
            Ok(0) => return Err(Error::Unavailable),
            Ok(n) => {
                let consumed = codec.push(&buffer[..n]).map_err(|_| Error::Protocol)?;
                if consumed != n {
                    return Err(Error::Protocol);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                wait_until(stream, false, cancelled)?
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => (),
            Err(_) => return Err(Error::Unavailable),
        }
    }
}

impl Client {
    /// Attach and authenticate. The wait has no time bound (owner, 2026-10-16):
    /// it ends on the HelloReply, a lost connection or `cancelled`.
    pub fn attach(
        runtime: &RuntimeDirectory,
        build: &str,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Self> {
        runtime.validate()?;
        let witness = runtime.capture_lock()?;
        let mut stream = match runtime.connect() {
            Ok(stream) => stream,
            Err(error @ (asura_platform::Error::Absent | asura_platform::Error::Refused)) => {
                if !witness.owner_released()? {
                    return Err(Error::Platform(asura_platform::Error::OwnerBusy));
                }
                return Err(Error::Platform(error));
            }
            Err(error) => return Err(error.into()),
        };
        let mut codec = ControlCodec::new();
        let hello = pb::Envelope {
            service_epoch: None,
            attachment_id: None,
            request_counter: None,
            body: Some(pb::envelope::Body::Hello(pb::Hello {
                client_build: Some(build.into()),
            })),
        };
        validate_semantics(&hello, Direction::ClientToServer).map_err(|_| Error::Protocol)?;
        write_frame_until(&mut stream, &hello, cancelled)?;
        let reply = match read_frame_until(&mut stream, &mut codec, cancelled)? {
            Frame::VersionRejected => return Err(Error::Incompatible),
            Frame::Message(reply) => reply,
        };
        validate_semantics(&reply, Direction::ServerToClient).map_err(|_| Error::Protocol)?;
        let Some(pb::envelope::Body::HelloReply(hello)) = reply.body else {
            return Err(Error::Protocol);
        };
        let epoch = reply
            .service_epoch
            .ok_or(Error::Protocol)?
            .try_into()
            .map_err(|_| Error::Protocol)?;
        let attachment = reply
            .attachment_id
            .ok_or(Error::Protocol)?
            .try_into()
            .map_err(|_| Error::Protocol)?;
        witness.validate()?;
        runtime.validate()?;
        Ok(Self {
            runtime: runtime.clone(),
            witness,
            stream,
            codec,
            epoch,
            attachment,
            counter: 0,
            build: hello.service_build.ok_or(Error::Protocol)?,
            abandoned: false,
            cancel: std::sync::Arc::new(never),
        })
    }

    /// Set what ends this attachment's waits besides a reply or a lost
    /// connection, for example the person's `/cancel` or exit. Pacing only.
    pub fn set_cancel(&mut self, cancel: Cancel) {
        self.cancel = cancel;
    }

    /// A read or control request. It has no time bound (owner, 2026-10-16):
    /// it ends on the reply, a lost connection or this attachment's cancel.
    fn request(&mut self, body: pb::envelope::Body) -> Result<pb::envelope::Body> {
        self.request_cancellable(body, &never)
    }
    /// As [`Self::request`], also ended by the caller's `cancelled`.
    fn request_cancellable(
        &mut self,
        body: pb::envelope::Body,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<pb::envelope::Body> {
        let cancel = self.cancel.clone();
        self.request_until(body, &|| cancelled() || cancel())
    }
    /// A journal-backed mutation: the service replies only after its journal
    /// append, which has no time bound (J1-0). The client waits for the real
    /// reply with no response budget. Only `cancelled` or a lost connection ends
    /// the wait, as `OutcomeUnconfirmed`; the caller retains its request ID.
    fn mutation(
        &mut self,
        body: pb::envelope::Body,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<pb::envelope::Body> {
        self.request_cancellable(body, cancelled)
            .map_err(|error| match error {
                Error::Remote(..) => error,
                _ => Error::OutcomeUnconfirmed,
            })
    }
    fn request_until(
        &mut self,
        body: pb::envelope::Body,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<pb::envelope::Body> {
        if self.abandoned {
            return Err(Error::Unavailable);
        }
        self.runtime.validate()?;
        self.witness.validate()?;
        self.counter = self.counter.checked_add(1).ok_or(Error::Protocol)?;
        let request = pb::Envelope {
            service_epoch: Some(self.epoch.to_vec()),
            attachment_id: Some(self.attachment.to_vec()),
            request_counter: Some(self.counter),
            body: Some(body),
        };
        validate_semantics(&request, Direction::ClientToServer).map_err(|_| Error::Protocol)?;
        // A cancelled or lost wait leaves a reply in flight: this attachment
        // is retired and refuses further requests.
        let result = write_frame_until(&mut self.stream, &request, cancelled)
            .and_then(|()| read_frame_until(&mut self.stream, &mut self.codec, cancelled));
        if result.is_err() {
            self.abandoned = true;
        }
        let Frame::Message(reply) = result? else {
            return Err(Error::Protocol);
        };
        validate_semantics(&reply, Direction::ServerToClient).map_err(|_| Error::Protocol)?;
        validate_identity(&reply, &self.epoch, &self.attachment, self.counter)
            .map_err(|_| Error::Protocol)?;
        self.runtime.validate()?;
        self.witness.validate()?;
        match reply.body.ok_or(Error::Protocol)? {
            pb::envelope::Body::Error(error) => Err(Error::Remote(
                error.code.ok_or(Error::Protocol)?,
                error.message.ok_or(Error::Protocol)?,
            )),
            body => Ok(body),
        }
    }

    pub fn observe_service(&mut self, after: Option<u64>) -> Result<ServiceUpdate> {
        match self.request(pb::envelope::Body::ObserveService(pb::ObserveService {
            after_revision: after,
        }))? {
            pb::envelope::Body::ServiceObservation(value) => {
                let status = value.status.ok_or(Error::Protocol)?;
                Ok(ServiceUpdate {
                    uptime_ms: value.uptime_ms.ok_or(Error::Protocol)?,
                    stored_memory: value.stored_memory.ok_or(Error::Protocol)?,
                    snapshot: Snapshot {
                        service_epoch: self.epoch,
                        service_build: self.build.clone(),
                        lifecycle: status.lifecycle.ok_or(Error::Protocol)?,
                        installation: status.installation.ok_or(Error::Protocol)?,
                        reason: status.reason.ok_or(Error::Protocol)?,
                        new_installation: status.new_installation == Some(true),
                    },
                    revision: value.revision.ok_or(Error::Protocol)?,
                    pending: value.pending.ok_or(Error::Protocol)?,
                    configured_model: value.configured_model,
                })
            }
            _ => Err(Error::Protocol),
        }
    }
    pub fn observe_queue(
        &mut self,
        project: [u8; 16],
        after: Option<u64>,
    ) -> Result<pb::ConversationQueueReply> {
        match self.request(pb::envelope::Body::ConversationQueueList(
            pb::ConversationQueueList {
                project_id: Some(project.to_vec()),
                input_id: None,
                after_revision: after,
            },
        ))? {
            pb::envelope::Body::ConversationQueueReply(value)
                if value.full_text == Some(false)
                    && value
                        .entries
                        .iter()
                        .all(|entry| entry.project_id.as_deref() == Some(project.as_slice())) =>
            {
                Ok(value)
            }
            _ => Err(Error::Protocol),
        }
    }
    pub fn inspect(&mut self) -> Result<Snapshot> {
        let reply = self.request(pb::envelope::Body::Inspect(pb::Inspect {}))?;
        let pb::envelope::Body::InspectReply(status) = reply else {
            return Err(Error::Protocol);
        };
        Ok(Snapshot {
            service_epoch: self.epoch,
            service_build: self.build.clone(),
            lifecycle: status.lifecycle.ok_or(Error::Protocol)?,
            installation: status.installation.ok_or(Error::Protocol)?,
            reason: status.reason.ok_or(Error::Protocol)?,
            new_installation: status.new_installation == Some(true),
        })
    }

    pub fn inspect_installation(&mut self) -> Result<InstallationSnapshot> {
        let reply = self.request(pb::envelope::Body::InspectInstallation(
            pb::InspectInstallation {},
        ))?;
        let pb::envelope::Body::InspectInstallationReply(status) = reply else {
            return Err(Error::Protocol);
        };
        Ok(InstallationSnapshot {
            service_epoch: self.epoch,
            installation: pb::InstallationState::try_from(
                status.installation.ok_or(Error::Protocol)?,
            )
            .map_err(|_| Error::Protocol)?,
            reason: pb::InstallationInspectionReason::try_from(
                status.reason.ok_or(Error::Protocol)?,
            )
            .map_err(|_| Error::Protocol)?,
            installation_id: status
                .installation_id
                .map(|id| id.try_into().map_err(|_| Error::Protocol))
                .transpose()?,
            authority_revision: status.authority_revision,
            recorded_owner_generation: status.recorded_owner_generation,
            binding_generation: status.binding_generation,
            authority_format: status.authority_format,
        })
    }

    /// Clone the transport solely to interrupt a worker-owned request on shutdown.
    /// Shutting down this handle also interrupts the original connection.
    pub fn disconnect_handle(&self) -> Result<std::os::unix::net::UnixStream> {
        self.stream
            .stream()
            .try_clone()
            .map_err(|error| Error::Platform(asura_platform::Error::from(error)))
    }

    /// Correlate a worker's immutable request scope with this attachment.
    pub fn service_epoch(&self) -> [u8; 16] {
        self.epoch
    }
    /// Read bounded committed audit metadata for the selected project. The
    /// wait ends on the reply, a lost connection or `cancelled`.
    pub fn audit(
        &mut self,
        query: pb::AuditRead,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<pb::AuditReply> {
        let project = query.project_id.clone();
        match self.request_cancellable(pb::envelope::Body::AuditRead(query), cancelled)? {
            pb::envelope::Body::AuditReply(reply) if reply.project_id == project => Ok(reply),
            _ => Err(Error::Protocol),
        }
    }

    /// Read one bounded page of durable sensor evidence without retrying.
    /// Call from the client control worker, never the render or input loop.
    pub fn sensors(
        &mut self,
        query: pb::SensorsInspect,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<pb::SensorsReply> {
        match self.request_cancellable(pb::envelope::Body::SensorsInspect(query), cancelled)? {
            pb::envelope::Body::SensorsReply(reply) => Ok(reply),
            _ => Err(Error::Protocol),
        }
    }

    /// Read the service-owned inventory without loading a model or retrying.
    /// The wait has no time bound; it ends on the reply, a lost connection or
    /// `cancelled` (the person's `/cancel` or exit).
    pub fn models(&mut self, cancelled: &dyn Fn() -> bool) -> Result<pb::ModelsReply> {
        match self
            .request_cancellable(pb::envelope::Body::ModelsList(pb::ModelsList {}), cancelled)?
        {
            pb::envelope::Body::ModelsReply(reply) => Ok(reply),
            _ => Err(Error::Protocol),
        }
    }

    /// Run one config request without retrying a possibly committed set.
    /// An empty key with no value reads the complete effective configuration.
    /// The wait has no time bound; `cancelled` or a lost connection ends it,
    /// and a set then reports `OutcomeUnconfirmed`.
    pub fn config(
        &mut self,
        key: &str,
        value: Option<&str>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<pb::ConfigReply> {
        use pb::envelope::Body;
        let body = match value {
            Some(value) => Body::ConfigSet(pb::ConfigSet {
                key: Some(key.into()),
                value_yaml: Some(value.into()),
            }),
            None => Body::ConfigGet(pb::ConfigGet {
                key: (!key.is_empty()).then(|| key.into()),
            }),
        };
        let result = self.request_cancellable(body, cancelled);
        match result {
            Ok(Body::ConfigReply(reply)) => Ok(reply),
            _ if value.is_some() => Err(Error::OutcomeUnconfirmed),
            Ok(_) => Err(Error::Protocol),
            Err(error) => Err(error),
        }
    }

    /// Mutations use caller-owned request IDs. An uncertain reply must be resolved
    /// with that same ID, never retried as a new operation.
    ///
    /// Initialization has no client response budget (owner, 2026-10-15): the
    /// client waits for the service's reply. Only `cancelled` returning true or
    /// a lost connection ends the wait, as `OutcomeUnconfirmed`; the caller
    /// retains the request ID and digest for resolution.
    pub fn initialize(
        &mut self,
        request: pb::InitializeInstallation,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<pb::InitializationReply> {
        match self.mutation(
            pb::envelope::Body::InitializeInstallation(request),
            cancelled,
        )? {
            pb::envelope::Body::InitializationReply(reply) => Ok(reply),
            _ => Err(Error::Protocol),
        }
    }
    pub fn resolve_initialization(
        &mut self,
        request: pb::ResolveInitialization,
    ) -> Result<pb::InitializationReply> {
        match self.request(pb::envelope::Body::ResolveInitialization(request))? {
            pb::envelope::Body::InitializationReply(reply) => Ok(reply),
            _ => Err(Error::Protocol),
        }
    }
    pub fn register_project(
        &mut self,
        request: pb::ProjectRegister,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<pb::ProjectReply> {
        match self.mutation(pb::envelope::Body::ProjectRegister(request), cancelled)? {
            pb::envelope::Body::ProjectReply(reply) => Ok(reply),
            _ => Err(Error::Protocol),
        }
    }
    pub fn rename_project(
        &mut self,
        request: pb::ProjectRename,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<pb::ProjectRenameReply> {
        let project_id = request.project_id.clone();
        match self.mutation(pb::envelope::Body::ProjectRename(request), cancelled)? {
            pb::envelope::Body::ProjectRenameReply(reply)
                if rename_reply_matches(&project_id, &reply) =>
            {
                Ok(reply)
            }
            _ => Err(Error::Protocol),
        }
    }
    pub fn projects(&mut self, after: Option<[u8; 16]>) -> Result<pb::ProjectsReply> {
        match self.request(pb::envelope::Body::ProjectList(pb::ProjectList {
            after_project_id: after.map(|id| id.to_vec()),
            limit: Some(8),
        }))? {
            pb::envelope::Body::ProjectsReply(reply) => Ok(reply),
            _ => Err(Error::Protocol),
        }
    }
    pub fn observe_context(
        &mut self,
        request: pb::ObserveContext,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<pb::ContextObservation> {
        let project = request.project_id.clone();
        let path = request.working_directory.clone();
        match self.request_cancellable(pb::envelope::Body::ObserveContext(request), cancelled)? {
            pb::envelope::Body::ContextObservation(reply)
                if reply.project_id == project && reply.working_directory == path =>
            {
                Ok(reply)
            }
            _ => Err(Error::Protocol),
        }
    }
    pub fn queue_conversation_input(
        &mut self,
        request: pb::ConversationQueueSubmit,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<pb::ConversationQueueReply> {
        let expected = request.clone();
        match self.mutation(
            pb::envelope::Body::ConversationQueueSubmit(request),
            cancelled,
        )? {
            pb::envelope::Body::ConversationQueueReply(reply)
                if queue_submit_reply_matches(&expected, &reply) =>
            {
                Ok(reply)
            }
            _ => Err(Error::Protocol),
        }
    }
    pub fn reorder_conversation_input(
        &mut self,
        request: pb::ConversationQueueReorder,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<pb::ConversationQueueReply> {
        let expected = request.clone();
        match self.mutation(
            pb::envelope::Body::ConversationQueueReorder(request),
            cancelled,
        )? {
            pb::envelope::Body::ConversationQueueReply(reply)
                if queue_reorder_reply_matches(&expected, &reply) =>
            {
                Ok(reply)
            }
            _ => Err(Error::Protocol),
        }
    }
    pub fn conversation_queue(
        &mut self,
        project: [u8; 16],
        input: Option<[u8; 16]>,
    ) -> Result<pb::ConversationQueueReply> {
        match self.request(pb::envelope::Body::ConversationQueueList(
            pb::ConversationQueueList {
                project_id: Some(project.to_vec()),
                input_id: input.map(|v| v.to_vec()),
                after_revision: None,
            },
        ))? {
            pb::envelope::Body::ConversationQueueReply(reply)
                if reply.full_text == Some(input.is_some())
                    && reply.entries.iter().all(|e| {
                        e.project_id.as_deref() == Some(project.as_slice())
                            && input.is_none_or(|id| e.input_id.as_deref() == Some(id.as_slice()))
                    }) =>
            {
                Ok(reply)
            }
            _ => Err(Error::Protocol),
        }
    }
    pub fn decide_conversation_input(
        &mut self,
        request: pb::ConversationQueueDecision,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<pb::ConversationQueueReply> {
        let expected = request.request_id.clone();
        match self.mutation(
            pb::envelope::Body::ConversationQueueDecision(request),
            cancelled,
        )? {
            pb::envelope::Body::ConversationQueueReply(reply) if reply.request_id == expected => {
                Ok(reply)
            }
            _ => Err(Error::Protocol),
        }
    }
    pub fn conversation_history(
        &mut self,
        project: [u8; 16],
        conversation: Option<[u8; 16]>,
        before_accepted_frame: Option<u64>,
        limit: u32,
    ) -> Result<pb::ConversationHistoryReply> {
        match self.request(pb::envelope::Body::ConversationHistory(
            pb::ConversationHistory {
                project_id: Some(project.to_vec()),
                conversation_id: conversation.map(|id| id.to_vec()),
                before_accepted_frame,
                limit: Some(limit),
            },
        ))? {
            pb::envelope::Body::ConversationHistoryReply(reply)
                if reply.project_id.as_deref() == Some(project.as_slice())
                    && (conversation.is_none()
                        || reply.conversation_id.as_deref()
                            == conversation.as_ref().map(|id| id.as_slice()))
                    && reply.entries.len() <= limit as usize
                    && reply.entries.iter().all(|e| {
                        before_accepted_frame.is_none_or(|before| {
                            e.accepted_frame.is_some_and(|frame| frame < before)
                        })
                    }) =>
            {
                Ok(reply)
            }
            _ => Err(Error::Protocol),
        }
    }
    pub fn read_conversation_prompt(
        &mut self,
        project: [u8; 16],
        operation: [u8; 16],
    ) -> Result<pb::ConversationPromptReply> {
        match self.request(pb::envelope::Body::ConversationReadPrompt(
            pb::ConversationReadPrompt {
                project_id: Some(project.to_vec()),
                operation_id: Some(operation.to_vec()),
            },
        ))? {
            pb::envelope::Body::ConversationPromptReply(reply)
                if reply.project_id.as_deref() == Some(project.as_slice())
                    && reply.operation_id.as_deref() == Some(operation.as_slice()) =>
            {
                Ok(reply)
            }
            _ => Err(Error::Protocol),
        }
    }
    /// One observation; `text_offset` is the count of response bytes held.
    pub fn observe_conversation(
        &mut self,
        operation: [u8; 16],
        after_cursor: u64,
        text_offset: u64,
    ) -> Result<pb::ConversationEvent> {
        match self.request(pb::envelope::Body::ConversationObserve(
            pb::ConversationObserve {
                operation_id: Some(operation.to_vec()),
                after_cursor: Some(after_cursor),
                wait_ms: Some(1000),
                text_offset: Some(text_offset),
            },
        ))? {
            pb::envelope::Body::ConversationEvent(reply)
                if reply.operation_id.as_deref() == Some(operation.as_slice())
                    && reply.cursor.is_some_and(|cursor| cursor >= after_cursor) =>
            {
                Ok(reply)
            }
            _ => Err(Error::Protocol),
        }
    }
    /// Observes `operation` until `assembly` holds the whole current
    /// response, then returns the last event carrying it whole. Each request
    /// ends on its reply, `cancel` or a lost connection.
    pub fn read_response(
        &mut self,
        operation: [u8; 16],
        assembly: &mut ResponseAssembly,
    ) -> Result<pb::ConversationEvent> {
        loop {
            let event = self.observe_conversation(operation, assembly.cursor(), assembly.held())?;
            assembly.apply(&event)?;
            if assembly.complete() {
                return Ok(assembly.whole(event));
            }
        }
    }
    /// The service acknowledges a live cancel after its durable record, so the
    /// wait has no client budget; `cancelled` stops only the wait.
    pub fn cancel_conversation(
        &mut self,
        operation: [u8; 16],
        generation: u64,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<pb::ConversationCancelAccepted> {
        match self.mutation(
            pb::envelope::Body::ConversationCancel(pb::ConversationCancel {
                operation_id: Some(operation.to_vec()),
                generation: Some(generation),
            }),
            cancelled,
        )? {
            pb::envelope::Body::ConversationCancelAccepted(reply)
                if reply.operation_id.as_deref() == Some(operation.as_slice()) =>
            {
                Ok(reply)
            }
            _ => Err(Error::Protocol),
        }
    }

    /// Stop the service this client attached to. The wait has no time bound
    /// (owner, 2026-10-16): it ends when the retained lock is released, a
    /// successor owner appears, the connection is lost before `StopAccepted`,
    /// or this attachment's cancel. Cancel reports `OutcomeUnconfirmed`.
    pub fn stop(self) -> Result<()> {
        self.stop_observed(&mut |_| ())
    }

    /// As [`Self::stop`], reporting progress while the service drains. About
    /// once per second (display pacing that ends nothing) a fresh attachment
    /// inspects the draining service; `progress` receives that snapshot.
    pub fn stop_observed(mut self, progress: &mut dyn FnMut(&Snapshot)) -> Result<()> {
        let body = pb::envelope::Body::Stop(pb::Stop {
            expected_epoch: Some(self.epoch.to_vec()),
        });
        let reply = self.request(body).map_err(|_| Error::OutcomeUnconfirmed)?;
        if !matches!(reply, pb::envelope::Body::StopAccepted(_)) {
            return Err(Error::OutcomeUnconfirmed);
        }
        let cancel = self.cancel.clone();
        let mut byte = [0];
        loop {
            match self.stream.stream_mut().read(&mut byte) {
                Ok(0) => break,
                Ok(_) => return Err(Error::OutcomeUnconfirmed),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    wait_until(&self.stream, false, &*cancel)
                        .map_err(|_| Error::OutcomeUnconfirmed)?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => (),
                Err(_) => return Err(Error::OutcomeUnconfirmed),
            }
        }
        // Lock polls pace at 10 ms; every 100th poll inspects for progress.
        let mut polls = 0u32;
        loop {
            if cancel() {
                return Err(Error::OutcomeUnconfirmed);
            }
            self.runtime
                .validate()
                .map_err(|_| Error::OutcomeUnconfirmed)?;
            if self
                .witness
                .owner_released()
                .map_err(|_| Error::OutcomeUnconfirmed)?
            {
                return Ok(());
            }
            polls = polls.wrapping_add(1);
            if polls.is_multiple_of(100)
                && let Ok(mut observer) = Client::attach(&self.runtime, &self.build, &*cancel)
            {
                if observer.epoch != self.epoch {
                    // A successor owner is never stopped by this request.
                    return Err(Error::OutcomeUnconfirmed);
                }
                observer.set_cancel(cancel.clone());
                if let Ok(snapshot) = observer.inspect() {
                    progress(&snapshot);
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

pub fn is_absent(error: &Error) -> bool {
    matches!(
        error,
        Error::Platform(asura_platform::Error::Absent | asura_platform::Error::Refused)
    )
}

/// Start only after an absent/refused attachment. A live incompatible owner stays untouched.
pub fn start(
    runtime: &RuntimeDirectory,
    build: &str,
    log: Option<&std::fs::File>,
) -> Result<(Client, bool)> {
    start_cancellable(runtime, build, log, &|| false)
}

/// Cancellation ends this client's startup wait; it never stops a shared service.
pub fn start_cancellable(
    runtime: &RuntimeDirectory,
    build: &str,
    log: Option<&std::fs::File>,
    cancelled: &dyn Fn() -> bool,
) -> Result<(Client, bool)> {
    start_internal(runtime, build, log, cancelled, None)
}

/// Spawned services follow this lifetime; an existing service is never adopted.
pub fn start_owned(
    runtime: &RuntimeDirectory,
    build: &str,
    log: Option<&std::fs::File>,
    cancelled: &dyn Fn() -> bool,
    lifetime: &asura_platform::ServiceLifetime,
) -> Result<(Client, bool)> {
    start_internal(runtime, build, log, cancelled, Some(lifetime))
}

fn start_internal(
    runtime: &RuntimeDirectory,
    build: &str,
    log: Option<&std::fs::File>,
    cancelled: &dyn Fn() -> bool,
    lifetime: Option<&asura_platform::ServiceLifetime>,
) -> Result<(Client, bool)> {
    let check = || {
        if cancelled() {
            Err(Error::Unavailable)
        } else {
            Ok(())
        }
    };
    check()?;
    // Startup has no time bound (owner, 2026-10-16). Each wait ends on the
    // attachment, a classified failure, child exit or the person's cancel.
    // The 50 ms sleeps are pacing and end nothing.
    match Client::attach(runtime, build, cancelled) {
        Ok(client) => return Ok((client, false)),
        Err(Error::Platform(asura_platform::Error::OwnerBusy)) => {
            // A held owner may still be binding. Wait without creating a second owner.
            loop {
                check()?;
                std::thread::sleep(Duration::from_millis(50));
                check()?;
                match Client::attach(runtime, build, cancelled) {
                    Ok(client) => return Ok((client, false)),
                    Err(Error::Platform(asura_platform::Error::OwnerBusy)) => (),
                    // The holder released the lock without serving: absence
                    // permits this client's one start attempt.
                    Err(error) if is_absent(&error) => break,
                    Err(error) => return Err(error),
                }
            }
        }
        Err(error) if is_absent(&error) => (),
        Err(error) => return Err(error),
    }
    check()?;
    let mut child = match lifetime {
        Some(lifetime) => asura_platform::spawn_service_owned(log, lifetime)?,
        None => asura_platform::spawn_service(log)?,
    };
    let mut notice_seen = false;
    let mut spawned = false;
    loop {
        check()?;
        if !notice_seen && let Some(notice) = child.poll_notice()? {
            match notice {
                StartupNotice::Bound => {
                    notice_seen = true;
                    spawned = true;
                }
                StartupNotice::OwnerBusy => notice_seen = true,
                StartupNotice::UnsafeRuntime => {
                    return Err(Error::Platform(asura_platform::Error::UnsafeRuntime));
                }
                _ => return Err(Error::Unavailable),
            }
        }
        let exited = child.try_reap()?.is_some();
        if notice_seen {
            match Client::attach(runtime, build, cancelled) {
                Ok(client) => return Ok((client, spawned)),
                // The winning owner holds the lock while it binds.
                Err(Error::Platform(asura_platform::Error::OwnerBusy)) => (),
                // Lock released with no endpoint: the owner ended.
                Err(error) if is_absent(&error) => return Err(Error::Unavailable),
                Err(error) => return Err(error),
            }
        } else if exited {
            return Err(Error::Unavailable);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rename_reply_requires_target_and_revision() {
        let target = Some(vec![2; 16]);
        let project = pb::ProjectReply {
            project_id: target.clone(),
            name: Some("Asura".into()),
            name_revision: Some(1),
            ..Default::default()
        };
        let good = pb::ProjectRenameReply {
            project: Some(project.clone()),
            current_project: Some(project.clone()),
            changed: Some(true),
            error: None,
        };
        assert!(rename_reply_matches(&target, &good));
        assert!(!rename_reply_matches(&Some(vec![3; 16]), &good));
        assert!(!rename_reply_matches(
            &target,
            &pb::ProjectRenameReply {
                project: None,
                ..good.clone()
            }
        ));
        assert!(!rename_reply_matches(
            &target,
            &pb::ProjectRenameReply {
                project: Some(pb::ProjectReply {
                    name_revision: None,
                    ..project.clone()
                }),
                ..good.clone()
            }
        ));
        assert!(rename_reply_matches(
            &target,
            &pb::ProjectRenameReply {
                project: Some(project.clone()),
                current_project: Some(project.clone()),
                changed: None,
                error: Some("stale_project_name_revision".into()),
            }
        ));
        assert!(!rename_reply_matches(
            &target,
            &pb::ProjectRenameReply {
                project: None,
                current_project: None,
                changed: None,
                error: Some("stale_project_name_revision".into()),
            }
        ));
        assert!(rename_reply_matches(
            &target,
            &pb::ProjectRenameReply {
                project: Some(project.clone()),
                current_project: Some(pb::ProjectReply {
                    name_revision: Some(2),
                    ..project.clone()
                }),
                changed: Some(true),
                error: None,
            }
        ));
        assert!(!rename_reply_matches(
            &target,
            &pb::ProjectRenameReply {
                project: Some(project.clone()),
                current_project: Some(pb::ProjectReply {
                    name_revision: Some(0),
                    ..project
                }),
                changed: Some(true),
                error: None,
            }
        ));
    }
    #[test]
    fn queue_receipts_bind_exact_request_and_new_input() {
        let submit = pb::ConversationQueueSubmit {
            request_id: Some(vec![1; 16]),
            project_id: Some(vec![2; 16]),
            conversation_id: Some(vec![3; 16]),
            expected_generation: Some(1),
            new_conversation: Some(false),
            prompt: Some("next".into()),
        };
        let mut reply = pb::ConversationQueueReply {
            entries: vec![pb::ConversationQueueEntry {
                input_id: Some(vec![4; 16]),
                project_id: submit.project_id.clone(),
                conversation_id: submit.conversation_id.clone(),
                ..Default::default()
            }],
            request_id: submit.request_id.clone(),
            full_text: Some(false),
            order_revision: Some(2),
            stale_order: Some(false),
            accepted_input_id: Some(vec![4; 16]),
            ..Default::default()
        };
        assert!(queue_submit_reply_matches(&submit, &reply));
        reply.accepted_input_id = Some(vec![5; 16]);
        assert!(!queue_submit_reply_matches(&submit, &reply));
        reply.accepted_input_id = Some(vec![4; 16]);
        reply.entries[0].conversation_id = Some(vec![8; 16]);
        assert!(!queue_submit_reply_matches(&submit, &reply));
        reply.entries[0].conversation_id = submit.conversation_id.clone();
        reply.entries.push(pb::ConversationQueueEntry {
            project_id: Some(vec![9; 16]),
            ..Default::default()
        });
        assert!(!queue_submit_reply_matches(&submit, &reply));
    }
    #[test]
    fn reorder_receipt_distinguishes_stale_projection() {
        let reorder = pb::ConversationQueueReorder {
            request_id: Some(vec![1; 16]),
            input_id: Some(vec![2; 16]),
            after_input_id: None,
            expected_order_revision: Some(1),
        };
        let mut reply = pb::ConversationQueueReply {
            request_id: reorder.request_id.clone(),
            full_text: Some(false),
            order_revision: Some(2),
            stale_order: Some(true),
            ..Default::default()
        };
        assert!(queue_reorder_reply_matches(&reorder, &reply));
        reply.stale_order = None;
        assert!(!queue_reorder_reply_matches(&reorder, &reply));
        reply.stale_order = Some(false);
        assert!(queue_reorder_reply_matches(&reorder, &reply));
        reply.accepted_input_id = Some(vec![3; 16]);
        assert!(!queue_reorder_reply_matches(&reorder, &reply));
    }
    #[test]
    fn held_lock_without_endpoint_is_unavailable_not_absent() {
        use std::os::unix::fs::DirBuilderExt;
        let path = std::path::PathBuf::from(format!(
            "/private/tmp/asura-client-{:x}",
            u128::from_ne_bytes(asura_platform::random_id())
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        let runtime = RuntimeDirectory::scratch(&path, true).unwrap();
        let owner = runtime.acquire_owner().unwrap();
        let error = match Client::attach(&runtime, "test/1", &|| false) {
            Ok(_) => panic!("unexpected attachment without listener"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            Error::Platform(asura_platform::Error::OwnerBusy)
        ));
        assert!(!is_absent(&error));
        assert!(matches!(
            start_cancellable(&runtime, "test/1", None, &|| true),
            Err(Error::Unavailable)
        ));
        drop(owner);
        assert!(matches!(
            start_cancellable(&runtime, "test/1", None, &|| true),
            Err(Error::Unavailable)
        ));
        let error = match Client::attach(&runtime, "test/1", &|| false) {
            Ok(_) => panic!("unexpected attachment without listener"),
            Err(error) => error,
        };
        assert!(is_absent(&error));
        std::fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn only_absence_permits_spawn() {
        assert!(is_absent(&Error::Platform(asura_platform::Error::Absent)));
        assert!(is_absent(&Error::Platform(asura_platform::Error::Refused)));
        for error in [
            Error::Unavailable,
            Error::Incompatible,
            Error::Protocol,
            Error::Platform(asura_platform::Error::UnsafeRuntime),
            Error::Platform(asura_platform::Error::OwnerBusy),
        ] {
            assert!(!is_absent(&error));
        }
    }
    #[test]
    fn wait_slice_paces_and_only_cancel_ends_it() {
        let cancelled = std::cell::Cell::new(false);
        let check = || cancelled.get();
        // The poll slice only paces cancel checks; it never ends the wait.
        for _ in 0..3 {
            assert_eq!(slice(&check).unwrap(), CANCEL_POLL);
        }
        cancelled.set(true);
        assert!(matches!(slice(&check), Err(Error::Cancelled)));
    }
    #[test]
    fn errors_do_not_disclose_paths() {
        let error = Error::Platform(asura_platform::Error::Io(std::io::Error::other(
            "private/path",
        )));
        assert_eq!(error.to_string(), "service_unavailable");
        assert_eq!(Error::Cancelled.to_string(), "cancelled");
    }

    /// How the fake service treats the request after Hello.
    #[derive(Clone, Copy)]
    enum Fake {
        /// Reply after holding the request this long.
        Hold(Duration),
        /// Never reply; keep the connection open until the test ends.
        Silent,
        /// Close the connection without replying.
        Close,
    }

    struct FakeService {
        path: std::path::PathBuf,
        runtime: RuntimeDirectory,
        server: Option<std::thread::JoinHandle<()>>,
        release: std::sync::mpsc::Sender<()>,
    }
    impl Drop for FakeService {
        fn drop(&mut self) {
            let _ = self.release.send(());
            if let Some(server) = self.server.take() {
                let _ = server.join();
            }
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn read_envelope(
        stream: &mut std::os::unix::net::UnixStream,
        codec: &mut ControlCodec,
    ) -> Option<pb::Envelope> {
        let mut buffer = [0u8; 8192];
        loop {
            if let Some(Frame::Message(envelope)) = codec.next_frame().ok()? {
                return Some(*envelope);
            }
            let n = stream.read(&mut buffer).ok()?;
            if n == 0 {
                return None;
            }
            codec.push(&buffer[..n]).ok()?;
        }
    }

    /// A real scratch runtime whose endpoint is served by a test thread that
    /// answers Hello, then treats the one following request as `mode` says.
    fn fake_service(mode: Fake) -> FakeService {
        use std::os::unix::fs::DirBuilderExt;
        let path = std::path::PathBuf::from(format!(
            "/private/tmp/asura-client-{:x}",
            u128::from_ne_bytes(asura_platform::random_id())
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        let runtime = RuntimeDirectory::scratch(&path, true).unwrap();
        let mut owner = runtime.acquire_owner().unwrap();
        let listener = owner.bind().unwrap();
        listener.set_nonblocking(false).unwrap();
        let (release, released) = std::sync::mpsc::channel::<()>();
        let server = std::thread::spawn(move || {
            let _owner = owner;
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_nonblocking(false).unwrap();
            let mut codec = ControlCodec::new();
            let epoch = [7u8; 16];
            let attachment = [9u8; 16];
            read_envelope(&mut stream, &mut codec).unwrap();
            let hello = pb::Envelope {
                service_epoch: Some(epoch.to_vec()),
                attachment_id: Some(attachment.to_vec()),
                request_counter: None,
                body: Some(pb::envelope::Body::HelloReply(pb::HelloReply {
                    service_build: Some("asura/fake".into()),
                    capabilities: vec![
                        pb::Capability::Inspect as i32,
                        pb::Capability::Stop as i32,
                        pb::Capability::InspectInstallation as i32,
                        pb::Capability::Config as i32,
                    ],
                    max_frame_bytes: Some(asura_control::MAX_FRAME_BYTES as u32),
                })),
            };
            stream.write_all(&encode_frame(&hello).unwrap()).unwrap();
            let Some(request) = read_envelope(&mut stream, &mut codec) else {
                return;
            };
            match mode {
                Fake::Close => drop(stream),
                Fake::Silent => {
                    let _ = released.recv();
                }
                Fake::Hold(hold) => {
                    std::thread::sleep(hold);
                    let body = match request.body {
                        Some(pb::envelope::Body::ConfigSet(_)) => {
                            pb::envelope::Body::ConfigReply(pb::ConfigReply {
                                value_yaml: Some("true".into()),
                                error: None,
                            })
                        }
                        _ => pb::envelope::Body::InspectReply(pb::InspectReply {
                            lifecycle: Some(pb::Lifecycle::Serving as i32),
                            installation: Some(pb::InstallationState::Recovering as i32),
                            reason: Some(
                                pb::InstallationInspectionReason::InspectionPending as i32,
                            ),
                            new_installation: None,
                        }),
                    };
                    let reply = pb::Envelope {
                        service_epoch: Some(epoch.to_vec()),
                        attachment_id: Some(attachment.to_vec()),
                        request_counter: request.request_counter,
                        body: Some(body),
                    };
                    let _ = stream.write_all(&encode_frame(&reply).unwrap());
                    let _ = released.recv();
                }
            }
        });
        FakeService {
            path,
            runtime,
            server: Some(server),
            release,
        }
    }

    #[test]
    fn cw01_held_reply_past_the_removed_read_bound_is_returned() {
        // The removed read bound was 2 s; every request shares this wait.
        let fake = fake_service(Fake::Hold(Duration::from_millis(2_500)));
        let mut client = Client::attach(&fake.runtime, "test/1", &|| false).unwrap();
        let snapshot = client.inspect().unwrap();
        assert_eq!(snapshot.lifecycle, pb::Lifecycle::Serving as i32);
    }

    #[test]
    fn cw02_cancel_ends_a_held_wait_and_retires_the_attachment() {
        let fake = fake_service(Fake::Silent);
        let mut client = Client::attach(&fake.runtime, "test/1", &|| false).unwrap();
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let setter = flag.clone();
        let cancel = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            setter.store(true, std::sync::atomic::Ordering::Release);
        });
        let started = std::time::Instant::now();
        let check = || flag.load(std::sync::atomic::Ordering::Acquire);
        // A set is a mutation: cancel reports an unconfirmed outcome.
        assert!(matches!(
            client.config("audit.enabled", Some("true"), &check),
            Err(Error::OutcomeUnconfirmed)
        ));
        assert!(started.elapsed() >= Duration::from_millis(300));
        cancel.join().unwrap();
        // The reply may still arrive: this attachment refuses further requests.
        assert!(matches!(client.inspect(), Err(Error::Unavailable)));
    }

    #[test]
    fn cw02_cancel_ends_a_held_read_as_cancelled() {
        let fake = fake_service(Fake::Silent);
        let mut client = Client::attach(&fake.runtime, "test/1", &|| false).unwrap();
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let setter = flag.clone();
        client.set_cancel(std::sync::Arc::new(move || {
            flag.load(std::sync::atomic::Ordering::Acquire)
        }));
        let cancel = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            setter.store(true, std::sync::atomic::Ordering::Release);
        });
        assert!(matches!(client.inspect(), Err(Error::Cancelled)));
        cancel.join().unwrap();
    }

    #[test]
    fn cw03_lost_connection_ends_the_wait() {
        let fake = fake_service(Fake::Close);
        let mut client = Client::attach(&fake.runtime, "test/1", &|| false).unwrap();
        assert!(matches!(client.inspect(), Err(Error::Unavailable)));
        let fake = fake_service(Fake::Close);
        let mut client = Client::attach(&fake.runtime, "test/1", &|| false).unwrap();
        assert!(matches!(
            client.config("audit.enabled", Some("true"), &|| false),
            Err(Error::OutcomeUnconfirmed)
        ));
    }
}
