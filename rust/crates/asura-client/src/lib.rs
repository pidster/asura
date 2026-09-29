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
    time::{Duration, Instant},
};

const REQUEST: Duration = Duration::from_secs(2);
const LIFECYCLE: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub enum Error {
    Platform(asura_platform::Error),
    Protocol,
    Incompatible,
    Unavailable,
    OutcomeUnconfirmed,
    Remote(i32, String),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Platform(asura_platform::Error::UnsafeRuntime) => "unsafe_runtime",
            Self::Incompatible => "incompatible_protocol",
            Self::OutcomeUnconfirmed => "outcome_unconfirmed",
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
}

fn remaining(end: Instant) -> Result<Duration> {
    end.checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or(Error::Unavailable)
}
fn wait(stream: &AuthenticatedStream, write: bool, end: Instant) -> Result<()> {
    let interest = PollInterest {
        fd: stream.stream().as_raw_fd(),
        read: !write,
        write,
    };
    asura_platform::poll(&[interest], remaining(end)?)?;
    Ok(())
}
fn write_frame(
    stream: &mut AuthenticatedStream,
    envelope: &pb::Envelope,
    end: Instant,
) -> Result<()> {
    let bytes = encode_frame(envelope).map_err(|_| Error::Protocol)?;
    let mut offset = 0;
    while offset < bytes.len() {
        remaining(end)?;
        match stream.stream_mut().write(&bytes[offset..]) {
            Ok(0) => return Err(Error::Unavailable),
            Ok(n) => offset += n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => wait(stream, true, end)?,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => (),
            Err(_) => return Err(Error::Unavailable),
        }
    }
    Ok(())
}
fn read_frame(
    stream: &mut AuthenticatedStream,
    codec: &mut ControlCodec,
    end: Instant,
) -> Result<Frame> {
    let mut buffer = [0u8; 8192];
    loop {
        remaining(end)?;
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
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => wait(stream, false, end)?,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => (),
            Err(_) => return Err(Error::Unavailable),
        }
    }
}

impl Client {
    pub fn attach(runtime: &RuntimeDirectory, build: &str, deadline: Instant) -> Result<Self> {
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
        let end = deadline.min(Instant::now() + REQUEST);
        write_frame(&mut stream, &hello, end)?;
        let reply = match read_frame(&mut stream, &mut codec, end)? {
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
        })
    }

    fn request(
        &mut self,
        body: pb::envelope::Body,
        deadline: Instant,
    ) -> Result<pb::envelope::Body> {
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
        write_frame(&mut self.stream, &request, deadline)?;
        let Frame::Message(reply) = read_frame(&mut self.stream, &mut self.codec, deadline)? else {
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
        match self.request(
            pb::envelope::Body::ObserveService(pb::ObserveService {
                after_revision: after,
            }),
            Instant::now() + REQUEST,
        )? {
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
        match self.request(
            pb::envelope::Body::ConversationQueueList(pb::ConversationQueueList {
                project_id: Some(project.to_vec()),
                input_id: None,
                after_revision: after,
            }),
            Instant::now() + REQUEST,
        )? {
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
        let reply = self.request(
            pb::envelope::Body::Inspect(pb::Inspect {}),
            Instant::now() + REQUEST,
        )?;
        let pb::envelope::Body::InspectReply(status) = reply else {
            return Err(Error::Protocol);
        };
        Ok(Snapshot {
            service_epoch: self.epoch,
            service_build: self.build.clone(),
            lifecycle: status.lifecycle.ok_or(Error::Protocol)?,
            installation: status.installation.ok_or(Error::Protocol)?,
            reason: status.reason.ok_or(Error::Protocol)?,
        })
    }

    pub fn inspect_installation(&mut self) -> Result<InstallationSnapshot> {
        let reply = self.request(
            pb::envelope::Body::InspectInstallation(pb::InspectInstallation {}),
            Instant::now() + REQUEST,
        )?;
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
    /// Read bounded committed audit metadata for the selected project.
    pub fn audit(&mut self, query: pb::AuditRead) -> Result<pb::AuditReply> {
        self.audit_before(query, Instant::now() + Duration::from_secs(2))
    }
    pub fn audit_before(
        &mut self,
        query: pb::AuditRead,
        deadline: Instant,
    ) -> Result<pb::AuditReply> {
        let project = query.project_id.clone();
        match self.request(pb::envelope::Body::AuditRead(query), deadline)? {
            pb::envelope::Body::AuditReply(reply) if reply.project_id == project => Ok(reply),
            _ => Err(Error::Protocol),
        }
    }

    /// Read one bounded page of durable sensor evidence without retrying.
    /// Call from the client control worker, never the render or input loop.
    pub fn sensors(&mut self, query: pb::SensorsInspect) -> Result<pb::SensorsReply> {
        match self.request(
            pb::envelope::Body::SensorsInspect(query),
            Instant::now() + Duration::from_secs(2),
        )? {
            pb::envelope::Body::SensorsReply(reply) => Ok(reply),
            _ => Err(Error::Protocol),
        }
    }

    /// Read the service-owned inventory without loading a model or retrying.
    pub fn models(&mut self) -> Result<pb::ModelsReply> {
        match self.request(
            pb::envelope::Body::ModelsList(pb::ModelsList {}),
            Instant::now() + Duration::from_secs(10),
        )? {
            pb::envelope::Body::ModelsReply(reply) => Ok(reply),
            _ => Err(Error::Protocol),
        }
    }

    /// Run one config request without retrying a possibly committed set.
    /// An empty key with no value reads the complete effective configuration.
    pub fn config(&mut self, key: &str, value: Option<&str>) -> Result<pb::ConfigReply> {
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
        let result = self.request(body, Instant::now() + Duration::from_secs(3));
        match result {
            Ok(Body::ConfigReply(reply)) => Ok(reply),
            _ if value.is_some() => Err(Error::OutcomeUnconfirmed),
            Ok(_) => Err(Error::Protocol),
            Err(error) => Err(error),
        }
    }

    /// Mutations use caller-owned request IDs. An uncertain reply must be resolved
    /// with that same ID, never retried as a new operation.
    pub fn initialize(
        &mut self,
        request: pb::InitializeInstallation,
    ) -> Result<pb::InitializationReply> {
        match self.request(
            pb::envelope::Body::InitializeInstallation(request),
            Instant::now() + Duration::from_secs(6),
        )? {
            pb::envelope::Body::InitializationReply(reply) => Ok(reply),
            _ => Err(Error::Protocol),
        }
    }
    pub fn resolve_initialization(
        &mut self,
        request: pb::ResolveInitialization,
    ) -> Result<pb::InitializationReply> {
        match self.request(
            pb::envelope::Body::ResolveInitialization(request),
            Instant::now() + REQUEST,
        )? {
            pb::envelope::Body::InitializationReply(reply) => Ok(reply),
            _ => Err(Error::Protocol),
        }
    }
    pub fn register_project(&mut self, request: pb::ProjectRegister) -> Result<pb::ProjectReply> {
        match self.request(
            pb::envelope::Body::ProjectRegister(request),
            Instant::now() + Duration::from_secs(3),
        )? {
            pb::envelope::Body::ProjectReply(reply) => Ok(reply),
            _ => Err(Error::Protocol),
        }
    }
    pub fn rename_project(&mut self, request: pb::ProjectRename) -> Result<pb::ProjectRenameReply> {
        let project_id = request.project_id.clone();
        match self.request(
            pb::envelope::Body::ProjectRename(request),
            Instant::now() + Duration::from_secs(3),
        )? {
            pb::envelope::Body::ProjectRenameReply(reply)
                if rename_reply_matches(&project_id, &reply) =>
            {
                Ok(reply)
            }
            _ => Err(Error::Protocol),
        }
    }
    pub fn projects(&mut self, after: Option<[u8; 16]>) -> Result<pb::ProjectsReply> {
        match self.request(
            pb::envelope::Body::ProjectList(pb::ProjectList {
                after_project_id: after.map(|id| id.to_vec()),
                limit: Some(8),
            }),
            Instant::now() + REQUEST,
        )? {
            pb::envelope::Body::ProjectsReply(reply) => Ok(reply),
            _ => Err(Error::Protocol),
        }
    }
    pub fn observe_context(
        &mut self,
        request: pb::ObserveContext,
    ) -> Result<pb::ContextObservation> {
        let project = request.project_id.clone();
        let path = request.working_directory.clone();
        match self.request(
            pb::envelope::Body::ObserveContext(request),
            Instant::now() + Duration::from_secs(2),
        )? {
            pb::envelope::Body::ContextObservation(reply)
                if reply.project_id == project && reply.working_directory == path =>
            {
                Ok(reply)
            }
            _ => Err(Error::Protocol),
        }
    }
    pub fn enqueue_conversation(
        &mut self,
        request: pb::ConversationEnqueue,
    ) -> Result<pb::ConversationQueueReply> {
        let expected = request.request_id.clone();
        match self.request(
            pb::envelope::Body::ConversationEnqueue(request),
            Instant::now() + Duration::from_secs(3),
        )? {
            pb::envelope::Body::ConversationQueueReply(reply) if reply.request_id == expected => {
                Ok(reply)
            }
            _ => Err(Error::Protocol),
        }
    }
    pub fn queue_conversation_input(
        &mut self,
        request: pb::ConversationQueueSubmit,
    ) -> Result<pb::ConversationQueueReply> {
        let expected = request.clone();
        match self.request(
            pb::envelope::Body::ConversationQueueSubmit(request),
            Instant::now() + Duration::from_secs(3),
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
    ) -> Result<pb::ConversationQueueReply> {
        let expected = request.clone();
        match self.request(
            pb::envelope::Body::ConversationQueueReorder(request),
            Instant::now() + Duration::from_secs(3),
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
        match self.request(
            pb::envelope::Body::ConversationQueueList(pb::ConversationQueueList {
                project_id: Some(project.to_vec()),
                input_id: input.map(|v| v.to_vec()),
                after_revision: None,
            }),
            Instant::now() + REQUEST,
        )? {
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
    ) -> Result<pb::ConversationQueueReply> {
        let expected = request.request_id.clone();
        match self.request(
            pb::envelope::Body::ConversationQueueDecision(request),
            Instant::now() + Duration::from_secs(3),
        )? {
            pb::envelope::Body::ConversationQueueReply(reply) if reply.request_id == expected => {
                Ok(reply)
            }
            _ => Err(Error::Protocol),
        }
    }
    pub fn submit_conversation(
        &mut self,
        request: pb::ConversationSubmit,
    ) -> Result<pb::ConversationAccepted> {
        let expected_generation = request.expected_generation.and_then(|n| n.checked_add(1));
        let expected_conversation = request.conversation_id.clone();
        match self.request(
            pb::envelope::Body::ConversationSubmit(request),
            Instant::now() + Duration::from_secs(3),
        )? {
            pb::envelope::Body::ConversationAccepted(reply)
                if reply.generation == expected_generation
                    && (expected_conversation.is_none()
                        || reply.conversation_id == expected_conversation) =>
            {
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
        match self.request(
            pb::envelope::Body::ConversationHistory(pb::ConversationHistory {
                project_id: Some(project.to_vec()),
                conversation_id: conversation.map(|id| id.to_vec()),
                before_accepted_frame,
                limit: Some(limit),
            }),
            Instant::now() + REQUEST,
        )? {
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
        match self.request(
            pb::envelope::Body::ConversationReadPrompt(pb::ConversationReadPrompt {
                project_id: Some(project.to_vec()),
                operation_id: Some(operation.to_vec()),
            }),
            Instant::now() + REQUEST,
        )? {
            pb::envelope::Body::ConversationPromptReply(reply)
                if reply.project_id.as_deref() == Some(project.as_slice())
                    && reply.operation_id.as_deref() == Some(operation.as_slice()) =>
            {
                Ok(reply)
            }
            _ => Err(Error::Protocol),
        }
    }
    pub fn observe_conversation(
        &mut self,
        operation: [u8; 16],
        after_cursor: u64,
    ) -> Result<pb::ConversationEvent> {
        match self.request(
            pb::envelope::Body::ConversationObserve(pb::ConversationObserve {
                operation_id: Some(operation.to_vec()),
                after_cursor: Some(after_cursor),
                wait_ms: Some(1000),
            }),
            Instant::now() + REQUEST,
        )? {
            pb::envelope::Body::ConversationEvent(reply)
                if reply.operation_id.as_deref() == Some(operation.as_slice())
                    && reply.cursor.is_some_and(|cursor| cursor >= after_cursor) =>
            {
                Ok(reply)
            }
            _ => Err(Error::Protocol),
        }
    }
    pub fn cancel_conversation(
        &mut self,
        operation: [u8; 16],
        generation: u64,
    ) -> Result<pb::ConversationCancelAccepted> {
        match self.request(
            pb::envelope::Body::ConversationCancel(pb::ConversationCancel {
                operation_id: Some(operation.to_vec()),
                generation: Some(generation),
            }),
            Instant::now() + Duration::from_secs(3),
        )? {
            pb::envelope::Body::ConversationCancelAccepted(reply)
                if reply.operation_id.as_deref() == Some(operation.as_slice()) =>
            {
                Ok(reply)
            }
            _ => Err(Error::Protocol),
        }
    }

    pub fn stop(mut self) -> Result<()> {
        let end = Instant::now() + LIFECYCLE;
        let body = pb::envelope::Body::Stop(pb::Stop {
            expected_epoch: Some(self.epoch.to_vec()),
        });
        let reply = self
            .request(body, end.min(Instant::now() + REQUEST))
            .map_err(|_| Error::OutcomeUnconfirmed)?;
        if !matches!(reply, pb::envelope::Body::StopAccepted(_)) {
            return Err(Error::OutcomeUnconfirmed);
        }
        let mut byte = [0];
        loop {
            if Instant::now() >= end {
                return Err(Error::OutcomeUnconfirmed);
            }
            match self.stream.stream_mut().read(&mut byte) {
                Ok(0) => break,
                Ok(_) => return Err(Error::OutcomeUnconfirmed),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    wait(&self.stream, false, end).map_err(|_| Error::OutcomeUnconfirmed)?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => (),
                Err(_) => return Err(Error::OutcomeUnconfirmed),
            }
        }
        while Instant::now() < end {
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
            std::thread::sleep(Duration::from_millis(10));
        }
        Err(Error::OutcomeUnconfirmed)
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
    let end = Instant::now() + LIFECYCLE;
    match Client::attach(runtime, build, end) {
        Ok(client) => return Ok((client, false)),
        Err(Error::Platform(asura_platform::Error::OwnerBusy)) => {
            // A held owner may still be binding. Wait without creating a second owner.
            loop {
                check()?;
                std::thread::sleep(Duration::from_millis(50).min(remaining(end)?));
                check()?;
                match Client::attach(runtime, build, end) {
                    Ok(client) => return Ok((client, false)),
                    Err(Error::Platform(asura_platform::Error::OwnerBusy)) => (),
                    Err(error) if is_absent(&error) => (),
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
        remaining(end)?;
        if !notice_seen && let Some(notice) = child.poll_notice(end)? {
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
            match Client::attach(runtime, build, end) {
                Ok(client) => return Ok((client, spawned)),
                Err(error) if is_absent(&error) => (),
                Err(error) => return Err(error),
            }
        } else if exited {
            return Err(Error::Unavailable);
        }
        std::thread::sleep(Duration::from_millis(50).min(remaining(end)?));
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
        let error = match Client::attach(&runtime, "test/1", Instant::now() + REQUEST) {
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
        let error = match Client::attach(&runtime, "test/1", Instant::now() + REQUEST) {
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
    fn deadlines_do_not_restart_and_errors_do_not_disclose_paths() {
        assert!(remaining(Instant::now() - Duration::from_millis(1)).is_err());
        let error = Error::Platform(asura_platform::Error::Io(std::io::Error::other(
            "private/path",
        )));
        assert_eq!(error.to_string(), "service_unavailable");
    }
}
