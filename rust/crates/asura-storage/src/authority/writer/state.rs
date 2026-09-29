use super::super::{HEADER, TRAILER};
use super::*;
use asura_platform::{AuthoritySource, random_id};
use std::sync::atomic::AtomicBool;

pub(super) struct State {
    runtime: RuntimeDirectory,
    file: Option<JournalFile>,
    bytes: Vec<u8>,
    replay: Option<Arc<Replay>>,
    fenced: bool,
    opened: bool,
    retired: Vec<std::sync::Weak<Replay>>,
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
impl State {
    pub(super) fn new(runtime: RuntimeDirectory) -> Self {
        Self {
            runtime,
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
    fn config(&self, deadline: Instant) -> Result<crate::config::ConversationSnapshot> {
        crate::config::conversation_snapshot(
            self.runtime.clone(),
            deadline,
            &AtomicBool::new(false),
        )
        .map_err(|_| Error::Invalid)
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
    fn append(&mut self, record: Record, deadline: Instant) -> Result<()> {
        let deadline = deadline.min(Instant::now() + Duration::from_secs(2));
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
        let proposed = replay(&self.bytes, deadline, &AtomicBool::new(false));
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
            .append(old, &frame, deadline)
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
    fn open(&mut self, deadline: Instant) -> Result<()> {
        if self.opened {
            return Ok(());
        }
        if self.replay.is_some() {
            return Err(Error::RepairRequired);
        }
        let scan = self
            .runtime
            .authority_source(deadline, &AtomicBool::new(false))
            .map_err(|_| Error::RepairRequired)?;
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
        let bytes = file.read_all(deadline).map_err(|_| Error::RepairRequired)?;
        let state =
            replay(&bytes, deadline, &AtomicBool::new(false)).map_err(|_| Error::RepairRequired)?;
        self.file = Some(file);
        self.bytes = bytes;
        self.replay = Some(Arc::new(state));
        self.append(Record::OwnerGeneration, deadline)?;
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
            self.append(
                Record::TurnTerminal(TurnTerminal {
                    operation: id,
                    generation,
                    kind: TerminalKind::Interrupted,
                    cause: Cause::Restart,
                    final_cursor: u64::MAX,
                    usage_known: !started,
                    output_tokens: 0,
                    charged_tokens: if started { reserved_output_tokens } else { 0 },
                    text: String::new(),
                }),
                deadline,
            )?;
        }
        if self.replay.as_ref().is_some_and(|r| r.binding.is_some()) {
            self.connect_graph(false, deadline)?;
        }
        self.opened = true;
        Ok(())
    }
    #[cfg(feature = "embedded-memory")]
    fn connect_graph(&mut self, initialize: bool, deadline: Instant) -> Result<()> {
        if let Some(graph) = &self.graph {
            graph
                .directory
                .validate()
                .map_err(|_| Error::RepairRequired)?;
            graph
                ._database
                .as_ref()
                .expect("live graph")
                .set_deadline(deadline);
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
            deadline,
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
    fn connect_graph(&mut self, _initialize: bool, _deadline: Instant) -> Result<()> {
        Err(Error::Unavailable)
    }
    fn initialize(&mut self, request: Id, deadline: Instant) -> Result<()> {
        if !cfg!(feature = "embedded-memory") {
            return Err(Error::Unavailable);
        }
        self.open(deadline)?;
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
                .authority_source(deadline, &AtomicBool::new(false))
                .map_err(|_| Error::RepairRequired)?;
            if !matches!(scan.source, AuthoritySource::RuntimeOnly) {
                return Err(Error::RepairRequired);
            }
            let configuration_digest = self.config(deadline)?.digest;
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
            let state =
                replay(&frame, deadline, &AtomicBool::new(false)).map_err(|_| Error::Invalid)?;
            let mut file =
                JournalFile::open(self.runtime.clone(), true).map_err(|_| Error::RepairRequired)?;
            if file
                .append(
                    0,
                    &frame,
                    deadline.min(Instant::now() + Duration::from_secs(2)),
                )
                .is_err()
            {
                self.fenced = true;
                self.file = Some(file);
                return Err(Error::OutcomeUnconfirmed);
            }
            self.file = Some(file);
            self.bytes = frame;
            self.replay = Some(Arc::new(state));
        }
        self.connect_graph(true, deadline)?;
        let state = self.replay.as_ref().expect("pending");
        self.append(
            Record::ActiveBinding(ActiveBinding {
                request,
                generation: 1,
                graph: state.initialization.graph,
                configuration_digest: state.initialization.configuration_digest,
            }),
            deadline,
        )
    }
    fn project(&self, id: Id) -> Result<ProjectIdentity> {
        let project = self
            .replay
            .as_ref()
            .ok_or(Error::NotInitialized)?
            .projects
            .get(&id)
            .ok_or(Error::Invalid)?;
        let held = ProjectIdentity::open(&project.location).map_err(|_| Error::StaleProject)?;
        if held.device != project.device || held.inode != project.inode {
            return Err(Error::StaleProject);
        }
        Ok(held)
    }
    fn input_view(&self, input: Id, full: bool) -> Result<InputView> {
        let state = self.replay.as_ref().ok_or(Error::NotInitialized)?;
        let entry = state.inputs.get(&input).ok_or(Error::Invalid)?;
        let (mut value, mut v2) = match self.record(entry.frame.clone())? {
            Record::InputQueued(value) => (value, None),
            Record::InputQueuedV2(p) => (
                InputQueued {
                    request: p.input,
                    digest: p.digest,
                    dispatch_request: p.dispatch_request,
                    project: p.project,
                    conversation: p.conversation,
                    target_operation: [0; 16],
                    target_generation: p.expected_generation,
                    previous: None,
                    kind: InputKind::Queue,
                    prompt: p.prompt.clone(),
                },
                Some(p),
            ),
            _ => return Err(Error::RepairRequired),
        };
        // The original record retains the enqueue digest and prompt. Scheduling
        // fields can change only through a durable promotion record.
        value.kind = entry.kind;
        value.target_operation = entry.target_operation;
        value.target_generation = entry.target_generation;
        value.previous = entry.previous;
        if !full && value.prompt.len() > 256 {
            let mut end = 256;
            while !value.prompt.is_char_boundary(end) {
                end -= 1;
            }
            value.prompt.truncate(end);
            if let Some(v2) = &mut v2 {
                v2.prompt = value.prompt.clone();
            }
        }
        Ok(InputView {
            input: value,
            v2,
            order_position: if entry.v2 {
                Some(u32::try_from(entry.order_position).map_err(|_| Error::Limit)?)
            } else {
                None
            },
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
        entries.sort_by_key(|q| {
            (
                q.conversation,
                q.v2,
                if q.v2 { q.order_position } else { q.sequence },
            )
        });
        if entries.len() > MAX_INPUTS {
            if let Some(id) = accepted {
                if let Some(position) = entries.iter().position(|q| q.request == id) {
                    if position >= MAX_INPUTS {
                        entries.remove(MAX_INPUTS - 1);
                    }
                }
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
    fn memory_read(
        &mut self,
        project: Id,
        query: crate::memory::ReadNotes,
        deadline: Instant,
    ) -> Result<Reply> {
        query.validate().map_err(|_| Error::Invalid)?;
        let replay = self.replay.as_ref().ok_or(Error::NotInitialized)?;
        if !replay.projects.contains_key(&project) {
            return Err(Error::Invalid);
        }
        if replay.project_memory_blocked(project) {
            return Err(Error::Busy);
        }
        self.connect_graph(false, deadline)?;
        let graph = self.graph.as_ref().ok_or(Error::Unavailable)?;
        let database = graph._database.as_ref().ok_or(Error::Unavailable)?;
        database.set_deadline(deadline);
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
        deadline: Instant,
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
        self.connect_graph(false, deadline)?;
        let graph = self.graph.as_ref().ok_or(Error::Unavailable)?;
        let database = graph._database.as_ref().ok_or(Error::Unavailable)?;
        database.set_deadline(deadline);
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
        deadline: Instant,
    ) -> Result<Reply> {
        let replay = self.replay.as_ref().ok_or(Error::NotInitialized)?;
        if !replay.projects.contains_key(&project) {
            return Err(Error::Invalid);
        }
        self.connect_graph(false, deadline)?;
        #[cfg(feature = "embedded-memory")]
        {
            let graph = self.graph.as_ref().ok_or(Error::Unavailable)?;
            let database = graph._database.as_ref().ok_or(Error::Unavailable)?;
            database.set_deadline(deadline);
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
    pub(super) fn execute(&mut self, command: Command, deadline: Instant) -> Result<Reply> {
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
                return self.memory_create(
                    turn,
                    generation,
                    ordinal,
                    Some(body),
                    &cancel,
                    deadline,
                );
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
                    deadline,
                );
            }
            #[cfg(feature = "embedded-memory")]
            Command::MemoryRead { project, query } => {
                return self.memory_read(project, query, deadline);
            }
            Command::SensorLoad { project } => return self.sensor(project, None, deadline),
            Command::SensorStore {
                expected_revision,
                state,
            } => return self.sensor(state.project, Some((expected_revision, state)), deadline),

            Command::Open => self.open(deadline)?,
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
                            q.v2,
                            if q.v2 { q.order_position } else { q.sequence },
                        )
                    });
                    entries.truncate(MAX_INPUTS);
                    for entry in entries {
                        reply.inputs.push(self.input_view(entry.request, false)?);
                    }
                }
                return Ok(reply);
            }
            Command::Enqueue {
                request,
                project,
                conversation,
                target_operation,
                target_generation,
                kind,
                prompt,
            } => {
                let digest = request_digest(Request::Queue {
                    project,
                    conversation,
                    target_operation,
                    target_generation,
                    kind,
                    prompt: &prompt,
                })
                .map_err(|_| Error::Invalid)?;
                let state = self.replay.as_ref().ok_or(Error::NotInitialized)?;
                if let Some(prior) = state.requests.get(&request) {
                    return if prior.digest == digest && prior.result == Outcome::Input(request) {
                        self.queue_reply(request)
                    } else {
                        Err(Error::Conflict)
                    };
                }
                if state.active_operation != Some(target_operation)
                    || state.operations.get(&target_operation).is_none_or(|op| {
                        op.conversation != conversation
                            || op.generation != target_generation
                            || op.terminal.is_some()
                            || op.cancel.is_some()
                    })
                {
                    return Err(Error::Conflict);
                }
                if state
                    .inputs
                    .values()
                    .filter(|q| {
                        matches!(
                            state.input_status(q.request),
                            Some(InputStatus::Queued | InputStatus::Held | InputStatus::Running)
                        )
                    })
                    .count()
                    >= MAX_INPUTS
                {
                    return Err(Error::Limit);
                }
                if kind == InputKind::Steer && state.steering_input(target_operation).is_some() {
                    return Err(Error::Busy);
                }
                let previous = if kind == InputKind::Queue {
                    state
                        .inputs
                        .values()
                        .filter(|q| {
                            q.conversation == conversation
                                && q.kind == InputKind::Queue
                                && matches!(
                                    state.input_status(q.request),
                                    Some(
                                        InputStatus::Queued
                                            | InputStatus::Held
                                            | InputStatus::Running
                                    )
                                )
                        })
                        .max_by_key(|q| q.sequence)
                        .map(|q| q.request)
                } else {
                    None
                };
                if kind == InputKind::Steer {
                    self.steering_prompt(target_operation, &prompt)?;
                }
                let held = self.project(project)?;
                held.validate().map_err(|_| Error::StaleProject)?;
                self.append(
                    Record::InputQueued(InputQueued {
                        request,
                        digest,
                        dispatch_request: random_id(),
                        project,
                        conversation,
                        target_operation,
                        target_generation,
                        previous,
                        kind,
                        prompt,
                    }),
                    deadline,
                )?;
                held.validate().map_err(|_| Error::OutcomeUnconfirmed)?;
                return self.queue_reply(request);
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
                if let Some(id) = conversation {
                    if !state.conversations.get(&id).is_some_and(|c| {
                        c.project == project && expected_generation <= c.generation
                    }) {
                        return Err(Error::Conflict);
                    }
                }
                let held = self.project(project)?;
                held.validate().map_err(|_| Error::StaleProject)?;
                let revision = state.order_revision.checked_add(1).ok_or(Error::Limit)?;
                self.append(
                    Record::InputQueuedV2(InputQueuedV2 {
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
                    }),
                    deadline,
                )?;
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
                if !q.v2
                    || !matches!(
                        state.input_status(input),
                        Some(InputStatus::Queued | InputStatus::Held)
                    )
                {
                    return Err(Error::Conflict);
                }
                if let Some(anchor) = after {
                    let p = state.inputs.get(&anchor).ok_or(Error::Invalid)?;
                    if !p.v2
                        || p.conversation != q.conversation
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
                self.append(
                    Record::InputReordered(InputReordered {
                        request,
                        digest,
                        input,
                        after,
                        expected_order_revision,
                        order_revision: revision,
                    }),
                    deadline,
                )?;
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
                self.append(
                    Record::InputDecision(InputDecision {
                        request,
                        digest,
                        input,
                        action,
                    }),
                    deadline,
                )?;
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
                let Record::InputQueued(original) = self.record(queued.frame.clone())? else {
                    return Err(Error::RepairRequired);
                };
                self.steering_prompt(target_operation, &original.prompt)?;
                let held = self.project(project)?;
                held.validate().map_err(|_| Error::StaleProject)?;
                self.append(
                    Record::InputPromoted(InputPromoted {
                        request,
                        digest,
                        input,
                        target_operation,
                        target_generation,
                    }),
                    deadline,
                )?;
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
                    Record::InputQueuedV2(value) if q.kind == InputKind::Steer => {
                        self.steering_prompt(q.target_operation, &value.prompt)?
                    }
                    Record::InputQueuedV2(value) => value.prompt,
                    _ => return Err(Error::RepairRequired),
                };
                let mut reply = self.execute(
                    Command::Prepare {
                        project: q.project,
                        conversation: Some(q.conversation),
                        expected_generation: generation,
                        prompt: prompt.clone(),
                    },
                    deadline,
                )?;
                reply.queued = Some(QueuedPreparation {
                    request: q.dispatch_request,
                    project: q.project,
                    conversation: q.conversation,
                    generation,
                    prompt,
                });
                return Ok(reply);
            }

            Command::Initialize { request } => self.initialize(request, deadline)?,
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
                    reply.projects.push((p.clone(), self.project(*id).is_ok()));
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
                self.connect_graph(false, deadline)?;
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
                self.connect_graph(false, deadline)?;
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
                self.connect_graph(false, deadline)?;
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
                let held = ProjectIdentity::open(&location).map_err(|_| Error::StaleProject)?;
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
                self.append(record, deadline)?;
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
                self.append(
                    Record::ProjectRenamed(ProjectRenamed {
                        request,
                        digest,
                        project,
                        expected_name_revision,
                        name_revision,
                        requested_name: name,
                        name: capitalized.clone(),
                    }),
                    deadline,
                )?;
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
                self.connect_graph(false, deadline)?;
                let held = self.project(project)?;
                request_digest(Request::Submit {
                    project,
                    conversation,
                    expected_generation,
                    prompt: &prompt,
                })
                .map_err(|_| Error::Invalid)?;
                let state = self.replay.as_ref().ok_or(Error::NotInitialized)?;
                let config = self.config(deadline)?;
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
                    prior.sort_by_key(|op| op.generation);
                    if prior.len() > 32 {
                        return Err(Error::Limit);
                    }
                    let mut size = prompt.len();
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
                        size += accepted.prompt.len() + terminal.text.len();
                        if size > 64 * 1024 {
                            return Err(Error::Limit);
                        }
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
                        || self.config(deadline)?.digest != fence.configuration_digest
                    {
                        return Err(Error::Conflict);
                    }
                    self.connect_graph(false, deadline)?;
                    Some(self.project(fence.project)?)
                } else {
                    None
                };
                self.append(record, deadline)?;
                if let Some(held) = held {
                    held.validate().map_err(|_| Error::OutcomeUnconfirmed)?;
                }
            }
        }
        Ok(self.reply())
    }
}
