use super::super::{HEADER, TRAILER};
use super::*;
use asura_platform::{AuthoritySource, random_id};
use std::sync::atomic::AtomicBool;
#[cfg(feature = "embedded-memory")]
use std::time::Duration;

pub(super) struct State {
    runtime: RuntimeDirectory,
    file: Option<JournalFile>,
    bytes: Vec<u8>,
    replay: Option<Arc<Replay>>,
    fenced: bool,
    opened: bool,
    retired: Vec<std::sync::Weak<Replay>>,
    /// The writer's close flag. Shutdown cancels replay and configuration reads;
    /// no journal step has a time bound (J1-0).
    cancel: Arc<AtomicBool>,
    #[cfg(feature = "embedded-memory")]
    graph: Option<Graph>,
}
#[cfg(feature = "embedded-memory")]
struct Graph {
    _database: Option<crate::memory::database::Database>,
    _runtime: tokio::runtime::Runtime,
    directory: asura_platform::DatabaseDirectory,
    _claim: crate::memory::owner::Claim,
}
#[cfg(feature = "embedded-memory")]
impl Drop for Graph {
    fn drop(&mut self) {
        drop(self._database.take());
        // Keep the engine executor and global claim until SDK close tasks settle.
        // Only this isolated worker waits; its public close method keeps polling.
        while self._runtime.metrics().num_alive_tasks() != 0 {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
/// An overlap with Asura state is reported as itself; other faults are stale.
fn project_error(error: asura_platform::Error) -> Error {
    match error {
        asura_platform::Error::OverlapsAsuraState => Error::OverlapsAsuraState,
        _ => Error::StaleProject,
    }
}
impl State {
    pub(super) fn new(runtime: RuntimeDirectory, cancel: Arc<AtomicBool>) -> Self {
        Self {
            runtime,
            cancel,
            file: None,
            bytes: Vec::new(),
            replay: None,
            fenced: false,
            opened: false,
            retired: Vec::new(),
            #[cfg(feature = "embedded-memory")]
            graph: None,
        }
    }
    fn reply(&self) -> Reply {
        Reply {
            #[cfg(feature = "embedded-memory")]
            memory_result: None,
            #[cfg(feature = "embedded-memory")]
            memory_create_result: None,
            sensor_state: None,
            replay: self.replay.clone(),
            record: None,
            history: None,
            prepared: None,
            projects: Vec::new(),
            inputs: Vec::new(),
            queued: None,
            order_revision: self.replay.as_ref().map(|s| s.order_revision),
            stale_order: false,
            accepted_input_id: None,
            project_name: None,
            project_name_revision: None,
            project_name_changed: None,
            stale_project_name_revision: false,
        }
    }
    fn config(&self) -> Result<crate::config::ConversationSnapshot> {
        crate::config::conversation_snapshot(self.runtime.clone(), &self.cancel)
            .map_err(|_| Error::Invalid)
    }
    /// A replay failure while closing is a cancel, not damage.
    fn replay_failed(&self) -> Error {
        if self.cancel.load(Ordering::Acquire) {
            Error::Cancelled
        } else {
            Error::RepairRequired
        }
    }
    fn record(&self, range: Range<usize>) -> Result<Record> {
        let bytes = self.bytes.get(range).ok_or(Error::Invalid)?;
        if bytes.len() < HEADER + TRAILER {
            return Err(Error::Invalid);
        }
        let kind = u16::from_be_bytes([bytes[10], bytes[11]]);
        decode_record(kind, &bytes[HEADER..bytes.len() - TRAILER])
            .map_err(|_| Error::RepairRequired)
    }
    /// Appends one record. The write and sync have no time bound (J1-0); only
    /// an actual I/O or validation error fences the writer.
    fn append(&mut self, record: Record) -> Result<()> {
        if self.fenced {
            return Err(Error::RepairRequired);
        }
        self.retired.retain(|state| state.strong_count() != 0);
        let state = self.replay.as_ref().ok_or(Error::NotInitialized)?;
        // One prior snapshot may remain held by the reactor. Reject before allocating
        // another replay if a caller retains more generations than that contract.
        if !self.retired.is_empty() && Arc::strong_count(state) > 1 {
            return Err(Error::Busy);
        }
        let old_snapshot = if Arc::strong_count(state) > 1 {
            Some(Arc::downgrade(state))
        } else {
            None
        };
        let context = FrameContext {
            installation_id: state.installation_id,
            transition_id: random_id(),
            sequence: state.sequence.checked_add(1).ok_or(Error::Limit)?,
            prior_digest: state.last_digest,
            expected_revision: state.revision,
            owner_generation: state
                .owner_generation
                .checked_add(u64::from(matches!(record, Record::OwnerGeneration)))
                .ok_or(Error::Limit)?,
        };
        let frame = encode_frame(&context, &record).map_err(|_| Error::Invalid)?;
        let old = self.bytes.len();
        if old
            .checked_add(frame.len())
            .is_none_or(|n| n > super::super::MAX_JOURNAL)
        {
            return Err(Error::Limit);
        }
        self.bytes
            .try_reserve_exact(frame.len())
            .map_err(|_| Error::Limit)?;
        self.bytes.extend_from_slice(&frame);
        let proposed = replay(&self.bytes, &AtomicBool::new(false));
        let proposed = match proposed {
            Ok(state) => state,
            Err(_) => {
                self.bytes.truncate(old);
                return Err(Error::Invalid);
            }
        };
        if self
            .file
            .as_mut()
            .ok_or(Error::NotInitialized)?
            .append(old, &frame)
            .is_err()
        {
            self.fenced = true;
            self.bytes.truncate(old);
            return Err(Error::OutcomeUnconfirmed);
        }
        if let Some(old) = old_snapshot {
            self.retired.push(old);
        }
        self.replay = Some(Arc::new(proposed));
        Ok(())
    }
    fn open(&mut self) -> Result<()> {
        if self.opened {
            return Ok(());
        }
        if self.replay.is_some() {
            return Err(Error::RepairRequired);
        }
        let scan = self
            .runtime
            .authority_source(&self.cancel)
            .map_err(|_| self.replay_failed())?;
        match scan.source {
            AuthoritySource::RuntimeOnly => {
                self.opened = true;
                return Ok(());
            }
            AuthoritySource::Journal(reader) if !reader.has_unknown_root_content() => {}
            _ => return Err(Error::RepairRequired),
        }
        let file =
            JournalFile::open(self.runtime.clone(), false).map_err(|_| Error::RepairRequired)?;
        let bytes = file
            .read_all(&self.cancel)
            .map_err(|_| self.replay_failed())?;
        let state = replay(&bytes, &self.cancel).map_err(|_| self.replay_failed())?;
        self.file = Some(file);
        self.bytes = bytes;
        self.replay = Some(Arc::new(state));
        self.append(Record::OwnerGeneration)?;
        if let Some(id) = self
            .replay
            .as_ref()
            .and_then(|r| r.active_operation)
            .filter(|id| {
                !self.replay.as_ref().expect("replay").operations[id]
                    .tools
                    .iter()
                    .any(|t| t.create.is_some() && t.result_frame.is_none())
            })
        {
            let op = &self.replay.as_ref().expect("replay").operations[&id];
            let generation = op.generation;
            let started = op.helper.is_some();
            let reserved_output_tokens = op.reserved_output_tokens;
            self.append(Record::TurnTerminal(TurnTerminal {
                operation: id,
                generation,
                kind: TerminalKind::Interrupted,
                cause: Cause::Restart,
                final_cursor: u64::MAX,
                usage_known: !started,
                output_tokens: 0,
                charged_tokens: if started { reserved_output_tokens } else { 0 },
                text: String::new(),
            }))?;
        }
        if self.replay.as_ref().is_some_and(|r| r.binding.is_some()) {
            self.connect_graph(false)?;
        }
        self.opened = true;
        Ok(())
    }
    #[cfg(feature = "embedded-memory")]
    fn connect_graph(&mut self, initialize: bool) -> Result<()> {
        if let Some(graph) = &self.graph {
            graph
                .directory
                .validate()
                .map_err(|_| Error::RepairRequired)?;
            graph
                ._runtime
                .block_on(graph._database.as_ref().expect("live graph").verify())
                .map_err(|_| Error::RepairRequired)?;
            return graph
                .directory
                .validate()
                .map_err(|_| Error::RepairRequired);
        }
        let state = self.replay.as_ref().ok_or(Error::NotInitialized)?;
        if state.initialization.mode != 1 || (!initialize && state.binding.is_none()) {
            return Err(Error::RepairRequired);
        }
        let binding = crate::memory::Binding {
            installation_id: crate::memory::Id::new(state.installation_id)
                .map_err(|_| Error::Invalid)?,
            graph_id: crate::memory::Id::new(state.initialization.graph)
                .map_err(|_| Error::Invalid)?,
            init_operation_id: crate::memory::Id::new(state.initialization.request)
                .map_err(|_| Error::Invalid)?,
        };
        let directory = asura_platform::DatabaseDirectory::open(self.runtime.clone(), initialize)
            .map_err(|_| Error::RepairRequired)?;
        let claim = crate::memory::owner::Claim::acquire_validated(directory.path())
            .map_err(|_| Error::Busy)?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(2)
            .thread_stack_size(10 * 1024 * 1024)
            .enable_time()
            .build()
            .map_err(|_| Error::Unavailable)?;
        let database = runtime.block_on(crate::memory::database::Database::connect_path(
            directory.path().to_owned(),
            binding,
            initialize,
        ));
        let database = match database {
            Ok(database) => database,
            Err(error) => {
                while runtime.metrics().num_alive_tasks() != 0 {
                    std::thread::sleep(Duration::from_millis(10));
                }
                return Err(if error == crate::memory::Error::OutcomeUnconfirmed {
                    Error::OutcomeUnconfirmed
                } else {
                    Error::RepairRequired
                });
            }
        };
        let graph = Graph {
            _database: Some(database),
            _runtime: runtime,
            directory,
            _claim: claim,
        };
        graph
            .directory
            .validate()
            .map_err(|_| Error::RepairRequired)?;
        self.graph = Some(graph);
        Ok(())
    }
    #[cfg(not(feature = "embedded-memory"))]
    fn connect_graph(&mut self, _initialize: bool) -> Result<()> {
        Err(Error::Unavailable)
    }
    /// Initialization has no time bound (owner, 2026-10-15): it waits for the
    /// engine; shutdown and real errors stop it.
    fn initialize(&mut self, request: Id) -> Result<()> {
        if !cfg!(feature = "embedded-memory") {
            return Err(Error::Unavailable);
        }
        self.open()?;
        let digest = request_digest(Request::Initialize { mode: 1 }).map_err(|_| Error::Invalid)?;
        if let Some(state) = &self.replay {
            if state.initialization.digest != digest {
                return Err(Error::Conflict);
            }
            if state.initialization.request != request {
                if state.requests.contains_key(&request) {
                    return Err(Error::Conflict);
                }
                return Err(if state.binding.is_some() {
                    Error::AlreadyInitialized
                } else {
                    Error::Busy
                });
            }
            if state.binding.is_some() {
                return Ok(());
            }
        } else {
            let scan = self
                .runtime
                .authority_source(&self.cancel)
                .map_err(|_| self.replay_failed())?;
            if !matches!(scan.source, AuthoritySource::RuntimeOnly) {
                return Err(Error::RepairRequired);
            }
            let configuration_digest = self.config()?.digest;
            let pending = Record::PendingInit(PendingInit {
                request,
                digest,
                mode: 1,
                configuration_revision: 1,
                configuration_digest,
                graph: random_id(),
            });
            let context = FrameContext {
                installation_id: random_id(),
                transition_id: random_id(),
                sequence: 1,
                prior_digest: [0; 32],
                expected_revision: 0,
                owner_generation: 1,
            };
            let frame = encode_frame(&context, &pending).map_err(|_| Error::Invalid)?;
            let state = replay(&frame, &AtomicBool::new(false)).map_err(|_| Error::Invalid)?;
            let mut file =
                JournalFile::open(self.runtime.clone(), true).map_err(|_| Error::RepairRequired)?;
            if file.append(0, &frame).is_err() {
                self.fenced = true;
                self.file = Some(file);
                return Err(Error::OutcomeUnconfirmed);
            }
            self.file = Some(file);
            self.bytes = frame;
            self.replay = Some(Arc::new(state));
        }
        self.connect_graph(true)?;
        let state = self.replay.as_ref().expect("pending");
        self.append(Record::ActiveBinding(ActiveBinding {
            request,
            generation: 1,
            graph: state.initialization.graph,
            configuration_digest: state.initialization.configuration_digest,
        }))
    }
    fn project(&self, id: Id) -> Result<ProjectIdentity> {
        let project = self
            .replay
            .as_ref()
            .ok_or(Error::NotInitialized)?
            .projects
            .get(&id)
            .ok_or(Error::Invalid)?;
        let held = ProjectIdentity::open(&project.location, &self.runtime.state_boundary())
            .map_err(project_error)?;
        if held.device != project.device || held.inode != project.inode {
            return Err(Error::StaleProject);
        }
        Ok(held)
    }
    fn input_view(&self, input: Id, full: bool) -> Result<InputView> {
        let state = self.replay.as_ref().ok_or(Error::NotInitialized)?;
        let entry = state.inputs.get(&input).ok_or(Error::Invalid)?;
        let Record::InputQueued(mut value) = self.record(entry.frame.clone())? else {
            return Err(Error::RepairRequired);
        };
        // The record retains the enqueue digest and prompt. Scheduling fields
        // change only through a durable promotion record.
        if !full && value.prompt.len() > 256 {
            let mut end = 256;
            while !value.prompt.is_char_boundary(end) {
                end -= 1;
            }
            value.prompt.truncate(end);
        }
        Ok(InputView {
            input: value,
            kind: entry.kind,
            target_operation: (entry.kind == InputKind::Steer).then_some(entry.target_operation),
            target_generation: entry.target_generation,
            order_position: u32::try_from(entry.order_position).map_err(|_| Error::Limit)?,
            status: state.input_status(input).ok_or(Error::RepairRequired)?,
            operation: state.input_operation(input),
            sequence: entry.sequence,
        })
    }
    fn queue_reply(&self, input: Id) -> Result<Reply> {
        let mut reply = self.reply();
        reply.inputs.push(self.input_view(input, false)?);
        Ok(reply)
    }
    fn project_queue_reply(&self, project: Id, accepted: Option<Id>) -> Result<Reply> {
        let state = self.replay.as_ref().ok_or(Error::NotInitialized)?;
        let mut reply = self.reply();
        reply.accepted_input_id = accepted;
        let mut entries: Vec<_> = state
            .inputs
            .values()
            .filter(|q| {
                q.project == project
                    && matches!(
                        state.input_status(q.request),
                        Some(InputStatus::Queued | InputStatus::Held | InputStatus::Running)
                    )
            })
            .collect();
        if let Some(id) = accepted {
            let accepted_entry = state.inputs.get(&id).ok_or(Error::Invalid)?;
            if accepted_entry.project != project {
                return Err(Error::Invalid);
            }
            if !entries.iter().any(|q| q.request == id) {
                entries.push(accepted_entry);
            }
        }
        entries.sort_by_key(|q| (q.conversation, q.order_position));
        if entries.len() > MAX_INPUTS {
            if let Some(id) = accepted
                && let Some(position) = entries.iter().position(|q| q.request == id)
                && position >= MAX_INPUTS
            {
                entries.remove(MAX_INPUTS - 1);
            }
            entries.truncate(MAX_INPUTS);
        }
        for q in entries {
            reply.inputs.push(self.input_view(q.request, false)?);
        }
        Ok(reply)
    }
    fn steering_prompt(&self, target: Id, instruction: &str) -> Result<String> {
        let state = self.replay.as_ref().ok_or(Error::NotInitialized)?;
        let op = state.operations.get(&target).ok_or(Error::Invalid)?;
        let Record::TurnAccepted(prior) = self.record(op.accepted_frame.clone())? else {
            return Err(Error::RepairRequired);
        };
        let prompt = format!(
            "Original request:\n{}\n\nSteering instruction:\n{}",
            prior.prompt, instruction
        );
        if prompt.len() > MAX_PROMPT {
            return Err(Error::Limit);
        }
        Ok(prompt)
    }
    #[cfg(feature = "embedded-memory")]
    fn memory_read(&mut self, project: Id, query: crate::memory::ReadNotes) -> Result<Reply> {
        query.validate().map_err(|_| Error::Invalid)?;
        let replay = self.replay.as_ref().ok_or(Error::NotInitialized)?;
        if !replay.projects.contains_key(&project) {
            return Err(Error::Invalid);
        }
        if replay.project_memory_blocked(project) {
            return Err(Error::Busy);
        }
        self.connect_graph(false)?;
        let graph = self.graph.as_ref().ok_or(Error::Unavailable)?;
        let database = graph._database.as_ref().ok_or(Error::Unavailable)?;
        let context = crate::memory::Id::new(project).map_err(|_| Error::Invalid)?;
        let result = graph._runtime.block_on(database.read(context, query));
        graph
            .directory
            .validate()
            .map_err(|_| Error::RepairRequired)?;
        let mut reply = self.reply();
        reply.memory_result = Some(result);
        Ok(reply)
    }
    #[cfg(feature = "embedded-memory")]
    fn memory_create(
        &mut self,
        turn: Id,
        generation: u64,
        ordinal: u32,
        body: Option<String>,
        cancel: &AtomicBool,
    ) -> Result<Reply> {
        let replay = self.replay.as_ref().ok_or(Error::NotInitialized)?;
        let op = replay.operations.get(&turn).ok_or(Error::Invalid)?;
        let tool = ordinal
            .checked_sub(1)
            .and_then(|index| op.tools.get(index as usize))
            .ok_or(Error::Invalid)?;
        let intent = tool.create.as_deref().ok_or(Error::Invalid)?.clone();
        if op.generation != generation
            || intent.operation != turn
            || intent.ordinal != ordinal
            || op.terminal.is_some()
            || tool.result_frame.is_some()
            || !replay.projects.contains_key(&intent.project)
        {
            return Err(Error::Conflict);
        }
        if body.is_some()
            && (replay.active_operation != Some(turn)
                || op.owner_generation != replay.owner_generation
                || op.helper.is_none()
                || op.cancel.is_some())
        {
            return Err(Error::Conflict);
        }
        let binding = crate::memory::Binding {
            installation_id: crate::memory::Id::new(replay.installation_id)
                .map_err(|_| Error::Invalid)?,
            graph_id: crate::memory::Id::new(replay.initialization.graph)
                .map_err(|_| Error::Invalid)?,
            init_operation_id: crate::memory::Id::new(replay.initialization.request)
                .map_err(|_| Error::Invalid)?,
        };
        let command = body
            .map(|body| crate::memory::create::command_for_intent(binding, &intent, body))
            .transpose();
        self.connect_graph(false)?;
        let graph = self.graph.as_ref().ok_or(Error::Unavailable)?;
        let database = graph._database.as_ref().ok_or(Error::Unavailable)?;
        let result = graph._runtime.block_on(async {
            if let Some(command) = command? {
                if let Some((source, _)) = command.source {
                    database
                        .get(command.binding.clone(), command.context_id, source)
                        .await?;
                }
                database.put_cancellable(command, cancel).await?;
            }
            database.resolve_create(&intent).await
        });
        graph
            .directory
            .validate()
            .map_err(|_| Error::RepairRequired)?;
        let mut reply = self.reply();
        reply.memory_create_result = Some(result);
        Ok(reply)
    }
    fn sensor(
        &mut self,
        project: Id,
        write: Option<(u64, crate::sensors::ProjectState)>,
    ) -> Result<Reply> {
        let replay = self.replay.as_ref().ok_or(Error::NotInitialized)?;
        if !replay.projects.contains_key(&project) {
            return Err(Error::Invalid);
        }
        self.connect_graph(false)?;
        #[cfg(feature = "embedded-memory")]
        {
            let graph = self.graph.as_ref().ok_or(Error::Unavailable)?;
            let database = graph._database.as_ref().ok_or(Error::Unavailable)?;
            let result = match write {
                Some((revision, state)) => graph
                    ._runtime
                    .block_on(database.sensor_store(revision, state)),
                None => graph._runtime.block_on(database.sensor_load(project)),
            }
            .map_err(|error| match error {
                crate::sensors::Error::Invalid => Error::Invalid,
                crate::sensors::Error::Limit => Error::Limit,
                crate::sensors::Error::Conflict | crate::sensors::Error::Stale => Error::Conflict,
                crate::sensors::Error::Unavailable => Error::Unavailable,
                crate::sensors::Error::Unconfirmed => Error::OutcomeUnconfirmed,
            })?;
            graph
                .directory
                .validate()
                .map_err(|_| Error::RepairRequired)?;
            let mut reply = self.reply();
            reply.sensor_state = Some(result);
            Ok(reply)
        }
        #[cfg(not(feature = "embedded-memory"))]
        {
            let _ = write;
            Err(Error::Unavailable)
        }
    }
    pub(super) fn execute(&mut self, command: Command) -> Result<Reply> {
        if self.fenced {
            return Err(Error::RepairRequired);
        }
        if self
            .file
            .as_ref()
            .is_some_and(|file| file.validate().is_err())
        {
            self.fenced = true;
            return Err(Error::RepairRequired);
        }
        match command {
            #[cfg(feature = "embedded-memory")]
            Command::MemoryCreate {
                turn,
                generation,
                ordinal,
                body,
                cancel,
            } => {
                return self.memory_create(turn, generation, ordinal, Some(body), &cancel);
            }
            #[cfg(feature = "embedded-memory")]
            Command::MemoryResolveCreate {
                turn,
                generation,
                ordinal,
            } => {
                return self.memory_create(
                    turn,
                    generation,
                    ordinal,
                    None,
                    &AtomicBool::new(false),
                );
            }
            #[cfg(feature = "embedded-memory")]
            Command::MemoryRead { project, query } => {
                return self.memory_read(project, query);
            }
            Command::SensorLoad { project } => return self.sensor(project, None),
            Command::SensorStore {
                expected_revision,
                state,
            } => return self.sensor(state.project, Some((expected_revision, state))),

            Command::Open => self.open()?,
            Command::Inputs { project, input } => {
                let state = self.replay.as_ref().ok_or(Error::NotInitialized)?;
                if !state.projects.contains_key(&project) {
                    return Err(Error::Invalid);
                }
                let mut reply = self.reply();
                if let Some(id) = input {
                    let view = self.input_view(id, true)?;
                    if view.input.project != project {
                        return Err(Error::Invalid);
                    }
                    reply.inputs.push(view);
                } else {
                    let mut entries: Vec<_> = state
                        .inputs
                        .values()
                        .filter(|q| q.project == project)
                        .collect();
                    entries.sort_by_key(|q| {
                        (
                            !matches!(
                                state.input_status(q.request),
                                Some(
                                    InputStatus::Queued | InputStatus::Held | InputStatus::Running
                                )
                            ),
                            q.conversation,
                            q.order_position,
                        )
                    });
                    entries.truncate(MAX_INPUTS);
                    for entry in entries {
                        reply.inputs.push(self.input_view(entry.request, false)?);
                    }
                }
                return Ok(reply);
            }
            Command::QueueInput {
                request,
                project,
                conversation,
                expected_generation,
                new_conversation,
                prompt,
            } => {
                let digest = request_digest(Request::QueueInput {
                    project,
                    conversation,
                    expected_generation,
                    new_conversation,
                    prompt: &prompt,
                })
                .map_err(|_| Error::Invalid)?;
                let state = self.replay.as_ref().ok_or(Error::NotInitialized)?;
                if let Some(prior) = state.requests.get(&request) {
                    return if prior.digest == digest {
                        if let Outcome::QueueInput {
                            input,
                            conversation: _,
                            order_revision,
                        } = prior.result
                        {
                            let project = self
                                .replay
                                .as_ref()
                                .and_then(|s| s.inputs.get(&input))
                                .ok_or(Error::Invalid)?
                                .project;
                            let mut reply = self.project_queue_reply(project, Some(input))?;
                            reply.order_revision = Some(order_revision);
                            Ok(reply)
                        } else {
                            Err(Error::Conflict)
                        }
                    } else {
                        Err(Error::Conflict)
                    };
                }
                if !state.projects.contains_key(&project) {
                    return Err(Error::Invalid);
                }
                if let Some(id) = conversation
                    && !state.conversations.get(&id).is_some_and(|c| {
                        c.project == project && expected_generation <= c.generation
                    })
                {
                    return Err(Error::Conflict);
                }
                let held = self.project(project)?;
                held.validate().map_err(|_| Error::StaleProject)?;
                let revision = state.order_revision.checked_add(1).ok_or(Error::Limit)?;
                self.append(Record::InputQueued(InputQueued {
                    request,
                    digest,
                    input: random_id(),
                    dispatch_request: random_id(),
                    project,
                    conversation: conversation.unwrap_or_else(random_id),
                    expected_generation,
                    new_conversation,
                    prompt,
                    order_revision: revision,
                }))?;
                held.validate().map_err(|_| Error::OutcomeUnconfirmed)?;
                let input = match self
                    .replay
                    .as_ref()
                    .and_then(|s| s.requests.get(&request))
                    .map(|r| &r.result)
                {
                    Some(Outcome::QueueInput { input, .. }) => *input,
                    _ => return Err(Error::RepairRequired),
                };
                return self.project_queue_reply(project, Some(input));
            }
            Command::ReorderInput {
                request,
                input,
                after,
                expected_order_revision,
            } => {
                let digest = request_digest(Request::ReorderInput {
                    input,
                    after,
                    expected_order_revision,
                })
                .map_err(|_| Error::Invalid)?;
                let state = self.replay.as_ref().ok_or(Error::NotInitialized)?;
                if let Some(prior) = state.requests.get(&request) {
                    return if prior.digest == digest {
                        if let Outcome::ReorderInput {
                            input,
                            order_revision,
                        } = prior.result
                        {
                            let project = self
                                .replay
                                .as_ref()
                                .and_then(|s| s.inputs.get(&input))
                                .ok_or(Error::Invalid)?
                                .project;
                            let mut reply = self.project_queue_reply(project, None)?;
                            reply.order_revision = Some(order_revision);
                            Ok(reply)
                        } else {
                            Err(Error::Conflict)
                        }
                    } else {
                        Err(Error::Conflict)
                    };
                }
                if expected_order_revision != state.order_revision {
                    let project = state.inputs.get(&input).ok_or(Error::Invalid)?.project;
                    let mut reply = self.project_queue_reply(project, None)?;
                    reply.stale_order = true;
                    return Ok(reply);
                }
                let q = state.inputs.get(&input).ok_or(Error::Invalid)?;
                if !matches!(
                    state.input_status(input),
                    Some(InputStatus::Queued | InputStatus::Held)
                ) {
                    return Err(Error::Conflict);
                }
                if let Some(anchor) = after {
                    let p = state.inputs.get(&anchor).ok_or(Error::Invalid)?;
                    if p.conversation != q.conversation
                        || !matches!(
                            state.input_status(anchor),
                            Some(InputStatus::Queued | InputStatus::Held)
                        )
                    {
                        return Err(Error::Conflict);
                    }
                }
                let project = q.project;
                let revision = state.order_revision.checked_add(1).ok_or(Error::Limit)?;
                let held = self.project(project)?;
                held.validate().map_err(|_| Error::StaleProject)?;
                self.append(Record::InputReordered(InputReordered {
                    request,
                    digest,
                    input,
                    after,
                    expected_order_revision,
                    order_revision: revision,
                }))?;
                held.validate().map_err(|_| Error::OutcomeUnconfirmed)?;
                return self.project_queue_reply(project, None);
            }
            Command::InputDecision {
                request,
                input,
                action,
            } => {
                let digest = request_digest(Request::InputDecision { input, action })
                    .map_err(|_| Error::Invalid)?;
                let state = self.replay.as_ref().ok_or(Error::NotInitialized)?;
                if let Some(prior) = state.requests.get(&request) {
                    return if prior.digest == digest
                        && prior.result == Outcome::InputDecision(input)
                    {
                        self.queue_reply(input)
                    } else {
                        Err(Error::Conflict)
                    };
                }
                let status = state.input_status(input).ok_or(Error::Invalid)?;
                if !(if action == InputAction::Hold {
                    status == InputStatus::Queued
                } else {
                    status == InputStatus::Held
                }) {
                    return Err(Error::Conflict);
                }
                self.append(Record::InputDecision(InputDecision {
                    request,
                    digest,
                    input,
                    action,
                }))?;
                return self.queue_reply(input);
            }
            Command::PromoteInput {
                request,
                input,
                target_operation,
                target_generation,
            } => {
                let digest = request_digest(Request::PromoteInput {
                    input,
                    target_operation,
                    target_generation,
                })
                .map_err(|_| Error::Invalid)?;
                let state = self.replay.as_ref().ok_or(Error::NotInitialized)?;
                if let Some(prior) = state.requests.get(&request) {
                    return if prior.digest == digest
                        && prior.result == Outcome::InputDecision(input)
                    {
                        self.queue_reply(input)
                    } else {
                        Err(Error::Conflict)
                    };
                }
                if !state.can_promote_input(input, target_operation, target_generation) {
                    return Err(Error::Conflict);
                }
                let queued = state.inputs.get(&input).ok_or(Error::Invalid)?;
                let project = queued.project;
                // Every queued input is a kind 18 record.
                let Record::InputQueued(original) = self.record(queued.frame.clone())? else {
                    return Err(Error::RepairRequired);
                };
                self.steering_prompt(target_operation, &original.prompt)?;
                let held = self.project(project)?;
                held.validate().map_err(|_| Error::StaleProject)?;
                self.append(Record::InputPromoted(InputPromoted {
                    request,
                    digest,
                    input,
                    target_operation,
                    target_generation,
                }))?;
                held.validate().map_err(|_| Error::OutcomeUnconfirmed)?;
                return self.queue_reply(input);
            }
            Command::PrepareInput { input } => {
                let state = self.replay.as_ref().ok_or(Error::NotInitialized)?;
                if state.ready_input() != Some(input) {
                    return Err(Error::Conflict);
                }
                let q = state.inputs.get(&input).ok_or(Error::Invalid)?.clone();
                let generation = state
                    .conversations
                    .get(&q.conversation)
                    .ok_or(Error::Invalid)?
                    .generation;
                let prompt = match self.record(q.frame)? {
                    Record::InputQueued(value) if q.kind == InputKind::Steer => {
                        self.steering_prompt(q.target_operation, &value.prompt)?
                    }
                    Record::InputQueued(value) => value.prompt,
                    _ => return Err(Error::RepairRequired),
                };
                let mut reply = self.execute(Command::Prepare {
                    project: q.project,
                    conversation: Some(q.conversation),
                    expected_generation: generation,
                    prompt: prompt.clone(),
                })?;
                reply.queued = Some(QueuedPreparation {
                    request: q.dispatch_request,
                    project: q.project,
                    conversation: q.conversation,
                    generation,
                    prompt,
                });
                return Ok(reply);
            }

            Command::Initialize { request } => self.initialize(request)?,
            Command::Projects { after, limit } => {
                if !(1..=8).contains(&limit) {
                    return Err(Error::Invalid);
                }
                let state = self.replay.as_ref().ok_or(Error::NotInitialized)?;
                let mut reply = self.reply();
                for (id, p) in state
                    .projects
                    .iter()
                    .filter(|(id, _)| after.is_none_or(|after| **id > after))
                    .take(limit)
                {
                    reply.projects.push((p.clone(), self.project(*id).err()));
                }
                return Ok(reply);
            }
            Command::ReadRecord { range } => {
                let valid = self.replay.as_ref().is_some_and(|r| {
                    r.inputs.values().any(|q| q.frame == range)
                        || r.operations.values().any(|op| {
                            op.accepted_frame == range
                                || op.terminal.as_ref().is_some_and(|t| t.frame == range)
                        })
                });
                if !valid {
                    return Err(Error::Invalid);
                }
                let mut reply = self.reply();
                reply.record = Some(self.record(range)?);
                return Ok(reply);
            }
            Command::History {
                project,
                conversation,
                before_accepted_frame,
                limit,
            } => {
                if !(1..=8).contains(&limit)
                    || before_accepted_frame == Some(0)
                    || (before_accepted_frame.is_some() && conversation.is_none())
                {
                    return Err(Error::Invalid);
                }
                self.connect_graph(false)?;
                let held = self.project(project)?;
                let state = self.replay.as_ref().ok_or(Error::NotInitialized)?;
                let selected = if let Some(conversation) = conversation {
                    if !state
                        .conversations
                        .get(&conversation)
                        .is_some_and(|c| c.project == project)
                    {
                        return Err(Error::Invalid);
                    }
                    Some(conversation)
                } else {
                    state.latest_conversation(project)
                };
                if let Some(before) = before_accepted_frame
                    && !state.operations.values().any(|op| {
                        Some(op.conversation) == selected && op.accepted_frame.start == before
                    })
                {
                    return Err(Error::Invalid);
                }
                let mut entries: Vec<_> = selected
                    .map(|conversation| {
                        state.accepted_page(conversation, before_accepted_frame, limit + 1)
                    })
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(operation, op)| HistoryEntry {
                        operation,
                        generation: op.generation,
                        accepted_frame: op.accepted_frame.start,
                        terminal: op.terminal.as_ref().map(|terminal| terminal.kind),
                    })
                    .collect();
                let has_more = entries.len() > limit;
                entries.truncate(limit);
                held.validate().map_err(|_| Error::StaleProject)?;
                let mut reply = self.reply();
                reply.history = Some(HistoryPage {
                    project,
                    conversation: selected,
                    generation: selected.map(|id| state.conversations[&id].generation),
                    revision: state.revision,
                    has_more,
                    entries,
                });
                return Ok(reply);
            }
            Command::ReadPrompt { project, operation } => {
                self.connect_graph(false)?;
                let held = self.project(project)?;
                let state = self.replay.as_ref().ok_or(Error::NotInitialized)?;
                let op = state.operations.get(&operation).ok_or(Error::Invalid)?;
                if !state
                    .conversations
                    .get(&op.conversation)
                    .is_some_and(|c| c.project == project)
                {
                    return Err(Error::Invalid);
                }
                let record = self.record(op.accepted_frame.clone())?;
                let Record::TurnAccepted(ref accepted) = record else {
                    return Err(Error::RepairRequired);
                };
                if accepted.project != project
                    || accepted.operation != operation
                    || accepted.conversation != op.conversation
                    || accepted.generation != op.generation
                {
                    return Err(Error::RepairRequired);
                }
                held.validate().map_err(|_| Error::StaleProject)?;
                let mut reply = self.reply();
                reply.record = Some(record);
                return Ok(reply);
            }
            Command::Register { request, location } => {
                self.connect_graph(false)?;
                let digest = request_digest(Request::Register {
                    location: &location,
                })
                .map_err(|_| Error::Invalid)?;
                let state = self.replay.as_ref().ok_or(Error::NotInitialized)?;
                if let Some(existing) = state.requests.get(&request) {
                    return if existing.digest == digest {
                        if let Outcome::Project(id) = existing.result {
                            self.project(id)?
                                .validate()
                                .map_err(|_| Error::StaleProject)?;
                        } else {
                            return Err(Error::Conflict);
                        }
                        Ok(self.reply())
                    } else {
                        Err(Error::Conflict)
                    };
                }
                let held = ProjectIdentity::open(&location, &self.runtime.state_boundary())
                    .map_err(project_error)?;
                let record = if let Some(existing) = state
                    .projects
                    .values()
                    .find(|p| p.device == held.device && p.inode == held.inode)
                {
                    self.project(existing.project)?
                        .validate()
                        .map_err(|_| Error::StaleProject)?;
                    Record::ProjectRequestAlias(ProjectRequestAlias {
                        request,
                        digest,
                        location,
                        project: existing.project,
                        registry_revision: existing.registry_revision,
                    })
                } else {
                    Record::ProjectRegistered(ProjectRegistered {
                        request,
                        digest,
                        project: random_id(),
                        location,
                        device: held.device,
                        inode: held.inode,
                        registry_revision: 1,
                        visibility: 1,
                    })
                };
                held.validate().map_err(|_| Error::StaleProject)?;
                self.append(record)?;
                held.validate().map_err(|_| Error::OutcomeUnconfirmed)?;
            }
            Command::RenameProject {
                request,
                project,
                expected_name_revision,
                name,
            } => {
                let digest = request_digest(Request::RenameProject {
                    project,
                    expected_name_revision,
                    name: &name,
                })
                .map_err(|_| Error::Invalid)?;
                let state = self.replay.as_ref().ok_or(Error::NotInitialized)?;
                if let Some(existing) = state.requests.get(&request) {
                    if existing.digest != digest {
                        return Err(Error::Conflict);
                    }
                    let Outcome::ProjectRename {
                        project: original_project,
                        name: original_name,
                        name_revision: original_revision,
                        changed,
                    } = &existing.result
                    else {
                        return Err(Error::Conflict);
                    };
                    if *original_project != project {
                        return Err(Error::Conflict);
                    }
                    let mut reply = self.reply();
                    reply.project_name = Some(original_name.clone());
                    reply.project_name_revision = Some(*original_revision);
                    reply.project_name_changed = Some(*changed);
                    return Ok(reply);
                }
                let held = self.project(project)?;
                let (current_name, current_revision) =
                    state.project_name(project).ok_or(Error::Invalid)?;
                if expected_name_revision != current_revision {
                    let mut reply = self.reply();
                    reply.project_name = Some(current_name);
                    reply.project_name_revision = Some(current_revision);
                    reply.project_name_changed = Some(false);
                    reply.stale_project_name_revision = true;
                    return Ok(reply);
                }
                let capitalized = capitalize_project_name(&name).map_err(|_| Error::Invalid)?;
                let name_revision = current_revision.checked_add(1).ok_or(Error::Limit)?;
                held.validate().map_err(|_| Error::StaleProject)?;
                self.append(Record::ProjectRenamed(ProjectRenamed {
                    request,
                    digest,
                    project,
                    expected_name_revision,
                    name_revision,
                    requested_name: name,
                    name: capitalized.clone(),
                }))?;
                held.validate().map_err(|_| Error::OutcomeUnconfirmed)?;
                let mut reply = self.reply();
                reply.project_name_changed = Some(current_name != capitalized);
                reply.project_name = Some(capitalized);
                reply.project_name_revision = Some(name_revision);
                return Ok(reply);
            }
            Command::Prepare {
                project,
                conversation,
                expected_generation,
                prompt,
            } => {
                self.connect_graph(false)?;
                let held = self.project(project)?;
                request_digest(Request::Submit {
                    project,
                    conversation,
                    expected_generation,
                    prompt: &prompt,
                })
                .map_err(|_| Error::Invalid)?;
                let state = self.replay.as_ref().ok_or(Error::NotInitialized)?;
                let config = self.config()?;
                let model = config.model;
                let asset_root = config.asset_root;
                let ollama_endpoint = config.ollama_endpoint;
                let model_capabilities = config.model_capabilities;
                let configuration_digest = config.digest;
                let mut history = Vec::new();
                if let Some(id) = conversation {
                    let conversation = state.conversations.get(&id).ok_or(Error::Invalid)?;
                    if conversation.project != project
                        || conversation.generation != expected_generation
                    {
                        return Err(Error::Conflict);
                    }
                    let mut prior: Vec<_> = state
                        .operations
                        .values()
                        .filter(|op| {
                            op.conversation == id
                                && op
                                    .terminal
                                    .as_ref()
                                    .is_some_and(|t| t.kind == TerminalKind::Complete)
                        })
                        .collect();
                    // Candidates for the context composer, newest first. The window is
                    // known only after Hello, so no turn-count or byte rule applies
                    // here; the journal's fixed capacity bounds the total text.
                    prior.sort_by_key(|op| std::cmp::Reverse(op.generation));
                    for op in prior {
                        let Record::TurnAccepted(accepted) =
                            self.record(op.accepted_frame.clone())?
                        else {
                            return Err(Error::RepairRequired);
                        };
                        let Record::TurnTerminal(terminal) =
                            self.record(op.terminal.as_ref().expect("complete").frame.clone())?
                        else {
                            return Err(Error::RepairRequired);
                        };
                        history.push((*accepted, terminal));
                    }
                } else if expected_generation != 0 {
                    return Err(Error::Conflict);
                }
                held.validate().map_err(|_| Error::StaleProject)?;
                let mut reply = self.reply();
                reply.prepared = Some(Prepared {
                    model,
                    asset_root,
                    ollama_endpoint,
                    model_capabilities,
                    configuration_digest,
                    history,
                });
                return Ok(reply);
            }
            Command::Append {
                expected_revision,
                record,
                admission,
            } => {
                if self.replay.as_ref().ok_or(Error::NotInitialized)?.revision != expected_revision
                {
                    return Err(Error::Conflict);
                }
                let held = if let Record::TurnAccepted(turn) = &record {
                    let fence = admission.ok_or(Error::Invalid)?;
                    if fence.project != turn.project
                        || fence.configuration_digest != turn.configuration_digest
                        || self.config()?.digest != fence.configuration_digest
                    {
                        return Err(Error::Conflict);
                    }
                    self.connect_graph(false)?;
                    Some(self.project(fence.project)?)
                } else {
                    None
                };
                self.append(record)?;
                if let Some(held) = held {
                    held.validate().map_err(|_| Error::OutcomeUnconfirmed)?;
                }
            }
        }
        Ok(self.reply())
    }
}
