use super::*;
use crate::authority::ReplayError;
use crate::authority::index::{Entry, Key, Tag};
use crate::authority::journal::{Failure, HoldReason, Journal, JournalStatus, Opened, Store};
use asura_platform::{AuthoritySource, JournalDirectory, random_id};
use std::sync::atomic::AtomicBool;
#[cfg(feature = "embedded-memory")]
use std::time::Duration;

/// `Prepare` returns history until the next turn would pass this much prompt
/// and response text (SJ-D10).
pub const PREPARE_TEXT: usize = 8 * 1024 * 1024;

pub(super) struct State {
    runtime: RuntimeDirectory,
    journal: Option<Journal<JournalDirectory>>,
    replay: Option<Arc<Replay>>,
    fenced: bool,
    opened: bool,
    retired: Vec<std::sync::Weak<Replay>>,
    /// The writer's close flag. Shutdown cancels replay and configuration reads;
    /// no journal step has a time bound (J1-0).
    cancel: Arc<AtomicBool>,
    /// Journal usage and hold state shared with the handle.
    status: Arc<std::sync::Mutex<JournalStatus>>,
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
fn failure(error: Failure) -> Error {
    match error {
        Failure::Invalid => Error::Invalid,
        Failure::Held(HoldReason::Quota) => Error::JournalQuota,
        Failure::Held(HoldReason::Disk) => Error::JournalDiskSpace,
        Failure::Exhausted => Error::Limit,
        Failure::Unconfirmed => Error::OutcomeUnconfirmed,
        Failure::Cancelled => Error::Cancelled,
        Failure::OwnerBusy => Error::Busy,
        _ => Error::RepairRequired,
    }
}
fn lookup(error: ReplayError) -> Error {
    match error {
        ReplayError::Cancelled => Error::Cancelled,
        _ => Error::RepairRequired,
    }
}
/// The index key range of one `(tag, id)` prefix.
fn prefix(tag: Tag, id: Id) -> (Key, Key) {
    (Key::new(tag, id, 0), Key::new(tag, id, u64::MAX))
}
/// The text of an operation a window reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stream {
    Response,
    Thinking,
}
impl Stream {
    fn of(self, op: &Operation) -> &Response {
        match self {
            Stream::Response => &op.response,
            Stream::Thinking => &op.thinking,
        }
    }
    fn tag(self) -> Tag {
        match self {
            Stream::Response => Tag::Piece,
            Stream::Thinking => Tag::Thinking,
        }
    }
}
impl State {
    pub(super) fn new(
        runtime: RuntimeDirectory,
        cancel: Arc<AtomicBool>,
        status: Arc<std::sync::Mutex<JournalStatus>>,
    ) -> Self {
        Self {
            runtime,
            cancel,
            status,
            journal: None,
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
            replay: self.replay.clone(),
            order_revision: self.replay.as_ref().map(|s| s.order_revision),
            ..Default::default()
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
    fn journal(&mut self) -> Result<&mut Journal<JournalDirectory>> {
        self.journal.as_mut().ok_or(Error::NotInitialized)
    }
    fn state(&self) -> Result<&Arc<Replay>> {
        self.replay.as_ref().ok_or(Error::NotInitialized)
    }
    /// Runs index reads against the current state.
    fn read<R>(
        &mut self,
        f: impl FnOnce(&Replay, &mut Store<'_, JournalDirectory>) -> std::result::Result<R, ReplayError>,
    ) -> Result<R> {
        let (state, mut store) = self.journal()?.store();
        let result = f(&state, &mut store);
        // A damaged frame, page or run fences admission (SJ-F11, SJ-F12);
        // evidence is preserved and nothing more is written.
        if matches!(result, Err(ReplayError::Corrupt)) {
            self.fenced = true;
        }
        result.map_err(lookup)
    }
    fn record(&mut self, locator: Locator) -> Result<Record> {
        self.read(|_, store| store.frame(&locator))
    }
    /// Publishes the shared status and reports whether it changed the hold.
    pub(super) fn publish_status(&self) -> bool {
        let Some(journal) = &self.journal else {
            return false;
        };
        let status = journal.status();
        let mut shared = self.status.lock().unwrap_or_else(|e| e.into_inner());
        let changed = shared.held != status.held;
        *shared = status;
        changed
    }
    /// Idle work between commands: one merge step. Returns true while work remains.
    pub(super) fn idle(&mut self) -> bool {
        if self.fenced {
            return false;
        }
        let Some(journal) = self.journal.as_mut() else {
            return false;
        };
        match journal.merge_step() {
            Ok(more) => more,
            Err(Failure::Held(_)) => false,
            Err(_) => {
                self.fenced = journal.fenced();
                false
            }
        }
    }
    pub(super) fn merge_pending(&self) -> bool {
        self.journal.as_ref().is_some_and(|j| j.merge_pending())
    }
    pub(super) fn held(&self) -> bool {
        self.journal.as_ref().is_some_and(|j| j.held().is_some())
    }
    /// Paced free-space recheck while held. Returns true when the hold ended.
    pub(super) fn recheck(&mut self) -> bool {
        let Some(journal) = self.journal.as_mut() else {
            return false;
        };
        let was = journal.held().is_some();
        let free = journal.recheck();
        was && free
    }
    pub(super) fn shutdown(&mut self) {
        if let Some(journal) = self.journal.as_mut() {
            journal.abandon_merge();
        }
    }
    /// Bytes `lo..hi` of a journaled response or thinking text, rebuilt
    /// from its pieces. A piece cut inside a UTF-8 character is damage.
    fn text_bytes(&mut self, stream: Stream, operation: Id, lo: u64, hi: u64) -> Result<Vec<u8>> {
        let pieces = self.pieces(stream, operation, lo, hi)?;
        let mut out = Vec::with_capacity(hi.saturating_sub(lo) as usize);
        for piece in pieces {
            let end = piece.start + piece.kept;
            if end <= lo || piece.start >= hi {
                continue;
            }
            let text = match (stream, self.record(piece.frame)?) {
                (Stream::Response, Record::ResponseText(segment)) => segment.text,
                (Stream::Response, Record::TurnTerminal(terminal)) => terminal.text,
                (Stream::Thinking, Record::ThinkingText(segment)) => segment.text,
                _ => return Err(Error::RepairRequired),
            };
            let kept = usize::try_from(piece.kept).map_err(|_| Error::RepairRequired)?;
            if kept > text.len() || !text.is_char_boundary(kept) {
                return Err(Error::RepairRequired);
            }
            let from = (lo.max(piece.start) - piece.start) as usize;
            let to = (hi.min(end) - piece.start) as usize;
            out.extend_from_slice(&text.as_bytes()[from..to]);
        }
        Ok(out)
    }
    /// The pieces of an operation's text that overlap `lo..hi`, from the
    /// live state or delta, otherwise from `P` or `H` entries in the runs.
    fn pieces(
        &mut self,
        stream: Stream,
        operation: Id,
        lo: u64,
        hi: u64,
    ) -> Result<Vec<ResponsePiece>> {
        if let Some(op) = self.state()?.operations.get(&operation) {
            return Ok(stream
                .of(op)
                .pieces
                .iter()
                .filter(|p| p.start + p.kept > lo && p.start < hi)
                .cloned()
                .collect());
        }
        let tag = stream.tag();
        self.read(|state, store| {
            let (lo_key, hi_key) = prefix(tag, operation);
            // The last piece that starts at or before `lo`.
            let mut out = Vec::new();
            let mut from = state
                .range(
                    &lo_key,
                    &Key::new(tag, operation, lo.saturating_add(1)),
                    true,
                    1,
                    store,
                )?
                .first()
                .map_or(lo_key, |(k, _)| *k);
            loop {
                let batch = state.range(&from, &hi_key, false, 64, store)?;
                let full = batch.len() == 64;
                for (key, value) in &batch {
                    if key.disc >= hi {
                        return Ok(out);
                    }
                    let (Entry::Piece { kept, frame } | Entry::Thinking { kept, frame }) =
                        Entry::decode(tag, value)?
                    else {
                        return Err(ReplayError::Corrupt);
                    };
                    out.push(ResponsePiece {
                        start: key.disc,
                        kept,
                        frame,
                    });
                }
                match batch.last() {
                    Some((key, _)) if full => {
                        from = Key::new(
                            tag,
                            operation,
                            key.disc.checked_add(1).ok_or(ReplayError::Corrupt)?,
                        )
                    }
                    _ => return Ok(out),
                }
            }
        })
    }
    /// The response length, terminal frame and accepted frame of a terminal
    /// operation.
    fn terminal_facts(&mut self, operation: Id) -> Result<(u64, Locator, Locator)> {
        if let Some(op) = self.state()?.operations.get(&operation) {
            let terminal = op.terminal.as_ref().ok_or(Error::Invalid)?;
            return Ok((op.response.len, terminal.frame, op.accepted_frame));
        }
        let facts = self
            .read(|state, store| state.operation_facts(operation, store))?
            .ok_or(Error::Invalid)?;
        Ok((facts.response_len, facts.terminal_frame, facts.accepted))
    }
    /// The journaled thinking length: the live value, or the end of the
    /// last `H` piece of a settled operation (a derived value).
    fn thinking_len(&mut self, operation: Id) -> Result<u64> {
        if let Some(op) = self.state()?.operations.get(&operation) {
            return Ok(op.thinking.len);
        }
        self.read(|state, store| {
            let (lo, hi) = prefix(Tag::Thinking, operation);
            Ok(match state.range(&lo, &hi, true, 1, store)?.first() {
                Some((key, value)) => {
                    let Entry::Thinking { kept, .. } = Entry::decode(Tag::Thinking, value)? else {
                        return Err(ReplayError::Corrupt);
                    };
                    key.disc.checked_add(kept).ok_or(ReplayError::Corrupt)?
                }
                None => 0,
            })
        })
    }
    /// The whole journaled response.
    pub(super) fn response_text(&mut self, operation: Id, length: u64) -> Result<String> {
        String::from_utf8(self.text_bytes(Stream::Response, operation, 0, length)?)
            .map_err(|_| Error::RepairRequired)
    }
    /// At most `budget` bytes (at most `MAX_WINDOW_TEXT`) from the largest character
    /// boundary not after `offset`. Returns the window start and text.
    fn text_window(
        &mut self,
        stream: Stream,
        operation: Id,
        length: u64,
        offset: u64,
        budget: usize,
    ) -> Result<(u64, String)> {
        let budget = budget.min(MAX_WINDOW_TEXT) as u64;
        let requested = offset.min(length);
        let lo = requested.saturating_sub(3);
        let fetched_end = length.min(requested + budget + 1);
        let bytes = self.text_bytes(stream, operation, lo, fetched_end)?;
        let continuation = |index: usize| bytes.get(index).is_some_and(|b| b & 0xC0 == 0x80);
        let mut start = (requested - lo) as usize;
        while start > 0 && continuation(start) {
            start -= 1;
        }
        let window_end = length.min(lo + start as u64 + budget);
        let mut end = (window_end - lo) as usize;
        while end > start && continuation(end) {
            end -= 1;
        }
        let text =
            String::from_utf8(bytes[start..end].to_vec()).map_err(|_| Error::RepairRequired)?;
        Ok((lo + start as u64, text))
    }
    /// The tool calls of an operation, from the live state or rebuilt from
    /// their frames through `C` entries.
    fn tools(&mut self, operation: Id) -> Result<Vec<ToolRecord>> {
        if let Some(op) = self.state()?.operations.get(&operation) {
            return Ok(op.tools.clone());
        }
        let facts = self
            .read(|state, store| state.operation_facts(operation, store))?
            .ok_or(Error::Invalid)?;
        self.read(|state, store| {
            let project = match state.find_conversation(facts.conversation, store)? {
                Some(c) => c.project,
                None => return Err(ReplayError::Corrupt),
            };
            let binding = [
                state.installation_id,
                state.initialization.graph,
                state.initialization.request,
            ];
            let (lo, hi) = prefix(Tag::Call, operation);
            let calls = state.range(&lo, &hi, false, MAX_TOOL_CALL_ORDINAL as usize, store)?;
            let mut tools = Vec::with_capacity(calls.len());
            for (_, value) in calls {
                let Entry::Call { intent, result, .. } = Entry::decode(Tag::Call, &value)? else {
                    return Err(ReplayError::Corrupt);
                };
                let intent = (intent, store.frame(&intent)?);
                // A `run` exec_cmd call's kind 26, found through its job.
                let job_intent = match &intent.1 {
                    Record::ToolCall(call)
                        if call.decision == ToolDecision::Run
                            && call.tool_operation == Some(ToolOperation::ExecCmd) =>
                    {
                        match state.job_state(
                            exec_cmd_job_id(call.operation, call.generation, call.ordinal),
                            store,
                        )? {
                            Some(JobState::Unsettled(job)) => Some(job.intent),
                            Some(JobState::Settled(job)) => Some(job.intent),
                            None => None,
                        }
                    }
                    _ => None,
                };
                let follow = match job_intent {
                    Some(at) => Some((at, store.frame(&at)?)),
                    None => None,
                };
                let result = if result.is_none() {
                    None
                } else {
                    Some((result, store.frame(&result)?))
                };
                tools.push(tool_from_frames(
                    state.installation_id,
                    binding,
                    project,
                    intent,
                    follow,
                    result,
                )?);
            }
            Ok(tools)
        })
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
        if !self.retired.is_empty() && Arc::strong_count(state) > 2 {
            return Err(Error::Busy);
        }
        let old_snapshot = (Arc::strong_count(state) > 2).then(|| Arc::downgrade(state));
        self.replay = None;
        let journal = self.journal.as_mut().ok_or(Error::NotInitialized)?;
        let result = journal.append(record, random_id());
        self.replay = Some(journal.state().clone());
        if journal.fenced() {
            self.fenced = true;
        }
        result.map_err(failure)?;
        if let Some(old) = old_snapshot {
            self.retired.push(old);
        }
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
        let directory = JournalDirectory::open(self.runtime.clone(), false, self.cancel.clone())
            .map_err(|e| match e {
                asura_platform::JournalError::OwnerBusy => Error::Busy,
                _ => Error::RepairRequired,
            })?;
        let mut journal = match Journal::open(directory, true, &self.cancel) {
            Ok(Opened::Journal(journal)) => *journal,
            Ok(Opened::Empty(_)) => return Err(Error::RepairRequired),
            Err(Failure::Cancelled) => return Err(Error::Cancelled),
            Err(Failure::OwnerBusy) => return Err(Error::Busy),
            Err(_) => return Err(self.replay_failed()),
        };
        if let Ok(quota) = crate::config::journal_quota(self.runtime.clone(), &self.cancel) {
            journal.set_quota(quota);
        }
        self.replay = Some(journal.state().clone());
        self.journal = Some(journal);
        self.append(Record::OwnerGeneration)?;
        let interrupted = self
            .replay
            .as_ref()
            .and_then(|r| r.active())
            .filter(|(_, op)| {
                !op.tools
                    .iter()
                    .any(|t| t.create.is_some() && t.result_frame.is_none())
            });
        if let Some((id, op)) = interrupted {
            let generation = op.generation;
            let started = op.helper.is_some();
            // The interrupted operation keeps its journaled response prefix.
            let text_offset = op.response.len;
            self.append(Record::TurnTerminal(TurnTerminal {
                operation: id,
                generation,
                kind: TerminalKind::Interrupted,
                cause: Cause::Restart,
                final_cursor: u64::MAX,
                usage_known: !started,
                output_tokens: 0,
                text_offset,
                text: String::new(),
            }))?;
        }
        if self.replay.as_ref().is_some_and(|r| r.binding.is_some()) {
            self.connect_graph(false)?;
        }
        self.opened = true;
        Ok(())
    }
    /// Initialization has no time bound (owner, 2026-10-15): it waits for the
    /// engine; shutdown and real errors stop it.
    fn initialize(&mut self, request: Id) -> Result<()> {
        if !cfg!(feature = "embedded-memory") {
            return Err(Error::Unavailable);
        }
        self.open()?;
        let digest = request_digest(Request::Initialize { mode: 1 }).map_err(|_| Error::Invalid)?;
        if let Some(state) = self.replay.clone() {
            if state.initialization.digest != digest {
                return Err(Error::Conflict);
            }
            if state.initialization.request != request {
                if self.read(|s, store| s.outcome(request, store))?.is_some() {
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
            let pending = PendingInit {
                request,
                digest,
                mode: 1,
                configuration_revision: 1,
                configuration_digest,
                graph: random_id(),
            };
            let context = FrameContext {
                installation_id: random_id(),
                transition_id: random_id(),
                sequence: 1,
                prior_digest: [0; 32],
                expected_revision: 0,
                owner_generation: 1,
            };
            let directory = JournalDirectory::open(self.runtime.clone(), true, self.cancel.clone())
                .map_err(|e| match e {
                    asura_platform::JournalError::OwnerBusy => Error::Busy,
                    _ => Error::RepairRequired,
                })?;
            let mut journal = match Journal::initialize(directory, &context, pending) {
                Ok(journal) => journal,
                Err(Failure::Unconfirmed) => {
                    self.fenced = true;
                    return Err(Error::OutcomeUnconfirmed);
                }
                Err(error) => return Err(failure(error)),
            };
            if let Ok(quota) = crate::config::journal_quota(self.runtime.clone(), &self.cancel) {
                journal.set_quota(quota);
            }
            self.replay = Some(journal.state().clone());
            self.journal = Some(journal);
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
    /// Any registered project, from the live state or the index.
    fn find_project(&mut self, id: Id) -> Result<ProjectEntry> {
        self.read(|state, store| state.find_project(id, store))?
            .ok_or(Error::Invalid)
    }
    fn project(&mut self, id: Id) -> Result<ProjectIdentity> {
        let project = self.find_project(id)?;
        self.held_identity(&project.registration)
    }
    fn held_identity(&self, project: &ProjectRegistered) -> Result<ProjectIdentity> {
        let held = ProjectIdentity::open(&project.location, &self.runtime.state_boundary())
            .map_err(project_error)?;
        if held.device != project.device || held.inode != project.inode {
            return Err(Error::StaleProject);
        }
        Ok(held)
    }
    /// The listing of one project: registration, effective name and stale check.
    fn listing(&mut self, entry: ProjectEntry) -> ProjectListing {
        let (name, name_revision) = match &entry.name {
            Some(name) => (name.name.clone(), name.revision),
            None => (
                default_project_name(entry.registration.project, &entry.registration.location),
                0,
            ),
        };
        let error = self.held_identity(&entry.registration).err();
        ProjectListing {
            registration: entry.registration,
            name,
            name_revision,
            error,
        }
    }
    fn input_view(&mut self, input: Id, full: bool) -> Result<InputView> {
        let state = self.state()?.clone();
        let entry = state.inputs.get(&input).ok_or(Error::Invalid)?;
        let Record::InputQueued(mut value) = self.record(entry.frame)? else {
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
    fn queue_reply(&mut self, input: Id) -> Result<Reply> {
        let mut reply = self.reply();
        reply.inputs.push(self.input_view(input, false)?);
        Ok(reply)
    }
    fn project_queue_reply(&mut self, project: Id, accepted: Option<Id>) -> Result<Reply> {
        let state = self.state()?.clone();
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
        if let Some(id) = accepted
            && let Some(accepted_entry) = state.inputs.get(&id)
        {
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
        let ids: Vec<Id> = entries.iter().map(|q| q.request).collect();
        for id in ids {
            reply.inputs.push(self.input_view(id, false)?);
        }
        Ok(reply)
    }
    /// The project of an input, from the live state or its kind 18 frame.
    fn input_project(&mut self, input: Id) -> Result<Id> {
        if let Some(q) = self.state()?.inputs.get(&input) {
            return Ok(q.project);
        }
        let entry =
            self.read(|state, store| state.entry(&Key::new(Tag::Input, input, 0), store))?;
        let Some(Entry::Input { queued, .. }) = entry else {
            return Err(Error::Invalid);
        };
        match self.record(queued)? {
            Record::InputQueued(value) => Ok(value.project),
            _ => Err(Error::RepairRequired),
        }
    }
    fn steering_prompt(&mut self, target: Id, instruction: &str) -> Result<String> {
        let accepted = match self.state()?.operations.get(&target) {
            Some(op) => op.accepted_frame,
            None => {
                self.read(|state, store| state.operation_facts(target, store))?
                    .ok_or(Error::Invalid)?
                    .accepted
            }
        };
        let Record::TurnAccepted(prior) = self.record(accepted)? else {
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
    #[cfg(feature = "embedded-memory")]
    fn memory_read(&mut self, project: Id, query: crate::memory::ReadNotes) -> Result<Reply> {
        query.validate().map_err(|_| Error::Invalid)?;
        self.find_project(project)?;
        let replay = self.replay.as_ref().ok_or(Error::NotInitialized)?;
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
        body: Option<(String, Option<Id>)>,
        cancel: &AtomicBool,
    ) -> Result<Reply> {
        let replay = self.replay.as_ref().ok_or(Error::NotInitialized)?;
        let op = replay.operations.get(&turn).ok_or(Error::Invalid)?;
        let tool = ordinal
            .checked_sub(1)
            .and_then(|index| op.tools.get(index as usize))
            .ok_or(Error::Invalid)?;
        let intent = tool.create.ok_or(Error::Invalid)?;
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
            .map(|(body, source)| {
                crate::memory::create::command_for_call(
                    binding,
                    &intent,
                    body,
                    source.map(crate::memory::Id::new).transpose()?,
                )
            })
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
        self.find_project(project)?;
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
    /// One page of registered projects in ID order: ascending after a
    /// cursor, or descending below `before`. The page count has no bound.
    fn projects(&mut self, after: Option<Id>, before: Option<Id>, limit: usize) -> Result<Reply> {
        let (lo, hi, descending) = match (after, before) {
            (Some(after), None) => (
                Key::new(Tag::Project, after, 1),
                Key::last(Tag::Project),
                false,
            ),
            (None, Some(before)) => (
                Key::first(Tag::Project),
                Key::new(Tag::Project, before, 0),
                true,
            ),
            (None, None) => (Key::first(Tag::Project), Key::last(Tag::Project), false),
            _ => return Err(Error::Invalid),
        };
        let found =
            self.read(|state, store| state.range(&lo, &hi, descending, limit + 1, store))?;
        let more = found.len() > limit;
        let mut reply = self.reply();
        for (key, _) in found.into_iter().take(limit) {
            let entry = self.find_project(key.id)?;
            let listing = self.listing(entry);
            reply.projects.push(listing);
        }
        reply.more = more;
        Ok(reply)
    }
    /// Completed prior turns, newest first, until the next turn would pass
    /// 8 MiB of prompt and response text; the rest are reported as omitted.
    fn history(&mut self, conversation: Id) -> Result<(Vec<(TurnAccepted, TurnTerminal)>, u32)> {
        let completed = self
            .read(|state, store| state.find_conversation(conversation, store))?
            .ok_or(Error::Invalid)?
            .completed;
        let mut history = Vec::new();
        let mut text = 0usize;
        let (lo, mut hi) = prefix(Tag::Turn, conversation);
        'pages: loop {
            let page = self.read(|state, store| state.range(&lo, &hi, true, 8, store))?;
            if page.is_empty() {
                break;
            }
            for (key, value) in &page {
                hi = *key;
                let Entry::Turn {
                    operation,
                    terminal,
                    accepted,
                    ..
                } = Entry::decode(Tag::Turn, value).map_err(lookup)?
                else {
                    return Err(Error::RepairRequired);
                };
                if terminal != TerminalKind::Complete {
                    continue;
                }
                let Record::TurnAccepted(prior) = self.record(accepted)? else {
                    return Err(Error::RepairRequired);
                };
                let (length, terminal_frame, _) = self.terminal_facts(operation)?;
                if text + prior.prompt.len() + length as usize > PREPARE_TEXT {
                    break 'pages;
                }
                let Record::TurnTerminal(mut terminal) = self.record(terminal_frame)? else {
                    return Err(Error::RepairRequired);
                };
                // The whole response: kind 24 segments and the tail.
                terminal.text = self.response_text(operation, length)?;
                terminal.text_offset = 0;
                text += prior.prompt.len() + terminal.text.len();
                history.push((*prior, terminal));
            }
        }
        let omitted = completed.saturating_sub(history.len() as u64);
        Ok((history, u32::try_from(omitted).map_err(|_| Error::Limit)?))
    }
    pub(super) fn execute(&mut self, command: Command) -> Result<Reply> {
        if self.fenced {
            return Err(Error::RepairRequired);
        }
        match command {
            #[cfg(feature = "embedded-memory")]
            Command::MemoryCreate {
                turn,
                generation,
                ordinal,
                body,
                source,
                cancel,
            } => {
                return self.memory_create(
                    turn,
                    generation,
                    ordinal,
                    Some((body, source)),
                    &cancel,
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
            Command::SetQuota { bytes } => {
                if let Some(journal) = self.journal.as_mut() {
                    journal.set_quota(bytes);
                    journal.recheck();
                }
            }
            Command::JournalStatus => {
                if let Some(journal) = self.journal.as_mut() {
                    journal.recheck();
                }
                let mut reply = self.reply();
                reply.status = self.journal.as_ref().map(|j| j.status());
                return Ok(reply);
            }
            Command::Lookup { operation } => {
                let state = self.state()?.clone();
                let view = match state.operations.get(&operation) {
                    Some(op) => OperationView {
                        conversation: op.conversation,
                        generation: op.generation,
                        terminal: op.terminal.as_ref().map(|t| (t.kind, t.cause)),
                        cancel: op.cancel,
                    },
                    None => {
                        let facts = self
                            .read(|state, store| state.operation_facts(operation, store))?
                            .ok_or(Error::Invalid)?;
                        OperationView {
                            conversation: facts.conversation,
                            generation: facts.generation,
                            terminal: Some((facts.terminal, facts.cause)),
                            cancel: None,
                        }
                    }
                };
                let mut reply = self.reply();
                reply.operation = Some(view);
                return Ok(reply);
            }
            Command::PayloadFacts { payloads } => {
                if payloads.len() > 128 {
                    return Err(Error::Invalid);
                }
                let mut facts = Vec::new();
                for payload in payloads {
                    if let Some(fact) = self.payload_fact(payload)? {
                        facts.push(fact);
                    }
                }
                let mut reply = self.reply();
                reply.payload_facts = facts;
                return Ok(reply);
            }
            Command::Jobs {
                scope,
                before,
                limit,
            } => {
                let (jobs, before_found) = self.read(|state, store| {
                    let found = match before {
                        Some(sequence) => state.job_at(scope, sequence, store)?,
                        None => false,
                    };
                    Ok((state.jobs(scope, before, limit, store)?, found))
                })?;
                let mut reply = self.reply();
                reply.jobs = jobs;
                reply.before_found = before_found;
                return Ok(reply);
            }
            Command::JobFacts { job } => {
                let records = self.read(|state, store| state.job_records(job, store))?;
                let mut reply = self.reply();
                reply.job_records = records.map(Box::new);
                return Ok(reply);
            }
            Command::Settlements {
                conversation,
                after,
                through,
                limit,
            } => {
                let jobs = self.read(|state, store| {
                    state.job_settlements(conversation, after, through, limit, store)
                })?;
                let mut reply = self.reply();
                reply.jobs = jobs;
                return Ok(reply);
            }
            Command::Inputs { project, input } => {
                self.find_project(project)?;
                let state = self.state()?.clone();
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
                    let ids: Vec<Id> = entries.iter().map(|q| q.request).collect();
                    for id in ids {
                        reply.inputs.push(self.input_view(id, false)?);
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
                if let Some(prior) = self.read(|state, store| state.outcome(request, store))? {
                    return if prior.digest == digest {
                        if let Outcome::QueueInput {
                            input,
                            conversation: _,
                            order_revision,
                        } = prior.result
                        {
                            let project = self.input_project(input)?;
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
                self.find_project(project)?;
                if let Some(id) = conversation {
                    let found = self.read(|state, store| state.find_conversation(id, store))?;
                    if !found.is_some_and(|c| {
                        c.project == project && expected_generation <= c.generation
                    }) {
                        return Err(Error::Conflict);
                    }
                }
                let held = self.project(project)?;
                held.validate().map_err(|_| Error::StaleProject)?;
                let revision = self
                    .state()?
                    .order_revision
                    .checked_add(1)
                    .ok_or(Error::Limit)?;
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
                let input = match self.state()?.requests.get(&request).map(|r| &r.result) {
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
                if let Some(prior) = self.read(|state, store| state.outcome(request, store))? {
                    return if prior.digest == digest {
                        if let Outcome::ReorderInput {
                            input,
                            order_revision,
                        } = prior.result
                        {
                            let project = self.input_project(input)?;
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
                let state = self.state()?.clone();
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
                if let Some(prior) = self.read(|state, store| state.outcome(request, store))? {
                    return if prior.digest == digest
                        && prior.result == Outcome::InputDecision(input)
                    {
                        self.queue_reply(input)
                    } else {
                        Err(Error::Conflict)
                    };
                }
                let status = self.state()?.input_status(input).ok_or(Error::Invalid)?;
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
                if let Some(prior) = self.read(|state, store| state.outcome(request, store))? {
                    return if prior.digest == digest
                        && prior.result == Outcome::InputDecision(input)
                    {
                        self.queue_reply(input)
                    } else {
                        Err(Error::Conflict)
                    };
                }
                let state = self.state()?.clone();
                if !state.can_promote_input(input, target_operation, target_generation) {
                    return Err(Error::Conflict);
                }
                let queued = state.inputs.get(&input).ok_or(Error::Invalid)?;
                let project = queued.project;
                // Every queued input is a kind 18 record.
                let Record::InputQueued(original) = self.record(queued.frame)? else {
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
                let state = self.state()?.clone();
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
            Command::Projects {
                after,
                before,
                limit,
            } => {
                if !(1..=8).contains(&limit) {
                    return Err(Error::Invalid);
                }
                self.state()?;
                return self.projects(after, before, limit);
            }
            Command::Project { project } => {
                self.state()?;
                let entry = self.read(|state, store| state.find_project(project, store))?;
                let mut reply = self.reply();
                if let Some(entry) = entry {
                    let listing = self.listing(entry);
                    reply.projects.push(listing);
                }
                return Ok(reply);
            }
            Command::Conversations {
                project,
                before,
                limit,
            } => {
                if !(1..=8).contains(&limit) {
                    return Err(Error::Invalid);
                }
                self.find_project(project)?;
                let hi = Key::new(
                    Tag::ProjectConversation,
                    project,
                    before.unwrap_or(u64::MAX),
                );
                let lo = Key::new(Tag::ProjectConversation, project, 0);
                let found =
                    self.read(|state, store| state.range(&lo, &hi, true, limit + 1, store))?;
                let mut reply = self.reply();
                reply.more = found.len() > limit;
                for (key, value) in found.into_iter().take(limit) {
                    let Entry::ProjectConversation { conversation } =
                        Entry::decode(Tag::ProjectConversation, &value).map_err(lookup)?
                    else {
                        return Err(Error::RepairRequired);
                    };
                    reply.conversations.push((conversation, key.disc));
                }
                return Ok(reply);
            }
            Command::ReadRecord { locator } => {
                // A caller's locator that names no frame is not damage.
                let (_, mut store) = self.journal()?.store();
                let record = store.frame(&locator).map_err(|_| Error::Invalid)?;
                if !matches!(
                    record,
                    Record::InputQueued(_) | Record::TurnAccepted(_) | Record::TurnTerminal(_)
                ) {
                    return Err(Error::Invalid);
                }
                let mut reply = self.reply();
                reply.record = Some(record);
                return Ok(reply);
            }
            Command::ReadResponse {
                operation,
                offset,
                thinking_offset,
            } => {
                let (length, terminal_frame, accepted_frame) = self.terminal_facts(operation)?;
                let Record::TurnTerminal(mut record) = self.record(terminal_frame)? else {
                    return Err(Error::RepairRequired);
                };
                let Record::TurnAccepted(accepted) = self.record(accepted_frame)? else {
                    return Err(Error::RepairRequired);
                };
                // The thinking window first; the response gets the rest of
                // the shared budget (thinking-level.md).
                let thinking_bytes = self.thinking_len(operation)?;
                let (thinking_start, thinking_text) = self.text_window(
                    Stream::Thinking,
                    operation,
                    thinking_bytes,
                    thinking_offset,
                    MAX_WINDOW_TEXT,
                )?;
                let budget = MAX_WINDOW_TEXT - thinking_text.len();
                let (start, text) =
                    self.text_window(Stream::Response, operation, length, offset, budget)?;
                record.text_offset = start;
                record.text = text;
                let tools = self.tools(operation)?;
                let mut reply = self.reply();
                reply.response_bytes = Some(length);
                reply.record = Some(Record::TurnTerminal(record));
                reply.tools = Some(tools);
                reply.thinking = Some(ThinkingWindow {
                    level: accepted.thinking_level,
                    request: accepted.thinking_request,
                    offset: thinking_start,
                    text: thinking_text,
                    bytes: thinking_bytes,
                });
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
                let selected = match conversation {
                    Some(conversation) => {
                        let found =
                            self.read(|state, store| state.find_conversation(conversation, store))?;
                        if !found.is_some_and(|c| c.project == project) {
                            return Err(Error::Invalid);
                        }
                        Some(conversation)
                    }
                    None => self.find_project(project)?.latest.map(|l| l.conversation),
                };
                let page = match selected {
                    Some(conversation) => {
                        self.history_page(project, conversation, before_accepted_frame, limit)?
                    }
                    None => (Vec::new(), false, None),
                };
                held.validate().map_err(|_| Error::StaleProject)?;
                let mut reply = self.reply();
                reply.history = Some(HistoryPage {
                    project,
                    conversation: selected,
                    generation: page.2,
                    revision: self.state()?.revision,
                    has_more: page.1,
                    entries: page.0,
                });
                return Ok(reply);
            }
            Command::ReadPrompt { project, operation } => {
                self.connect_graph(false)?;
                let held = self.project(project)?;
                let (conversation, generation, accepted) =
                    match self.state()?.operations.get(&operation) {
                        Some(op) => (op.conversation, op.generation, op.accepted_frame),
                        None => {
                            let facts = self
                                .read(|state, store| state.operation_facts(operation, store))?
                                .ok_or(Error::Invalid)?;
                            (facts.conversation, facts.generation, facts.accepted)
                        }
                    };
                let found =
                    self.read(|state, store| state.find_conversation(conversation, store))?;
                if !found.is_some_and(|c| c.project == project) {
                    return Err(Error::Invalid);
                }
                let record = self.record(accepted)?;
                let Record::TurnAccepted(ref value) = record else {
                    return Err(Error::RepairRequired);
                };
                if value.project != project
                    || value.operation != operation
                    || value.conversation != conversation
                    || value.generation != generation
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
                if let Some(existing) = self.read(|state, store| state.outcome(request, store))? {
                    return if existing.digest == digest {
                        let Outcome::Project(id) = existing.result else {
                            return Err(Error::Conflict);
                        };
                        self.project(id)?
                            .validate()
                            .map_err(|_| Error::StaleProject)?;
                        let entry = self.find_project(id)?;
                        let mut reply = self.reply();
                        reply.project = Some(self.listing(entry));
                        Ok(reply)
                    } else {
                        Err(Error::Conflict)
                    };
                }
                let held = ProjectIdentity::open(&location, &self.runtime.state_boundary())
                    .map_err(project_error)?;
                let identity = Key::new(
                    Tag::Identity,
                    crate::authority::index::identity_id(held.device, held.inode),
                    0,
                );
                let existing = match self.read(|state, store| state.entry(&identity, store))? {
                    Some(Entry::Identity { project }) => Some(self.find_project(project)?),
                    Some(_) => return Err(Error::RepairRequired),
                    None => None,
                };
                let project_id = existing.as_ref().map(|e| e.registration.project);
                let record = if let Some(existing) = existing {
                    self.project(existing.registration.project)?
                        .validate()
                        .map_err(|_| Error::StaleProject)?;
                    Record::ProjectRequestAlias(ProjectRequestAlias {
                        request,
                        digest,
                        location,
                        project: existing.registration.project,
                        registry_revision: existing.registration.registry_revision,
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
                let id = match &record {
                    Record::ProjectRegistered(p) => p.project,
                    _ => project_id.expect("alias names a project"),
                };
                held.validate().map_err(|_| Error::StaleProject)?;
                self.append(record)?;
                held.validate().map_err(|_| Error::OutcomeUnconfirmed)?;
                let entry = self.find_project(id)?;
                let mut reply = self.reply();
                reply.project = Some(self.listing(entry));
                return Ok(reply);
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
                if let Some(existing) = self.read(|state, store| state.outcome(request, store))? {
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
                    let entry = self.find_project(project)?;
                    reply.project = Some(self.listing(entry));
                    return Ok(reply);
                }
                let entry = self.find_project(project)?;
                let held = self.held_identity(&entry.registration)?;
                let listing = self.listing(entry);
                let (current_name, current_revision) =
                    (listing.name.clone(), listing.name_revision);
                if expected_name_revision != current_revision {
                    let mut reply = self.reply();
                    reply.project_name = Some(current_name);
                    reply.project_name_revision = Some(current_revision);
                    reply.project_name_changed = Some(false);
                    reply.stale_project_name_revision = true;
                    reply.project = Some(listing);
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
                let entry = self.find_project(project)?;
                reply.project = Some(self.listing(entry));
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
                let config = self.config()?;
                let mut history = Vec::new();
                let mut omitted = 0;
                let notice = if let Some(id) = conversation {
                    let found = self
                        .read(|state, store| state.find_conversation(id, store))?
                        .ok_or(Error::Invalid)?;
                    if found.project != project || found.generation != expected_generation {
                        return Err(Error::Conflict);
                    }
                    (history, omitted) = self.history(id)?;
                    // The notice read: P from the previous kind 21, S the
                    // last committed sequence, on this worker (J5 AE-01).
                    let after = found.latest.map_or(0, |turn| turn.notice_through);
                    self.read(|state, store| state.job_notice(id, after, store))?
                } else if expected_generation != 0 {
                    return Err(Error::Conflict);
                } else {
                    JobNotice {
                        through: self.state()?.sequence,
                        ..JobNotice::default()
                    }
                };
                held.validate().map_err(|_| Error::StaleProject)?;
                let mut reply = self.reply();
                reply.prepared = Some(Prepared {
                    model: config.model,
                    asset_root: config.asset_root,
                    ollama_endpoint: config.ollama_endpoint,
                    model_capabilities: config.model_capabilities,
                    thinking: config.thinking,
                    configuration_digest: config.digest,
                    history,
                    omitted,
                    notice,
                });
                return Ok(reply);
            }
            Command::Append {
                expected_revision,
                record,
                admission,
            } => {
                if self.state()?.revision != expected_revision {
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
                // The kind 30 append-time bounds (16 notes, 64 records per
                // job); replay does not count them.
                if let Record::JobRetention(retention) = &record {
                    let summary = self
                        .read(|state, store| state.job_retention_summary(retention.job, store))?;
                    if !summary.has_room(retention.action) {
                        return Err(Error::Limit);
                    }
                }
                self.append(record)?;
                if let Some(held) = held {
                    held.validate().map_err(|_| Error::OutcomeUnconfirmed)?;
                }
            }
        }
        Ok(self.reply())
    }
    /// One newest-first page of a conversation's turns below an accepted
    /// sequence (SJ-D14), with the conversation generation.
    fn history_page(
        &mut self,
        project: Id,
        conversation: Id,
        before: Option<u64>,
        limit: usize,
    ) -> Result<(Vec<HistoryEntry>, bool, Option<u64>)> {
        let found = self
            .read(|state, store| state.find_conversation(conversation, store))?
            .ok_or(Error::Invalid)?;
        let state = self.state()?.clone();
        let mut entries = Vec::new();
        // The newest turn may be active (no `T` entry yet).
        let active = state
            .active()
            .filter(|(_, op)| op.conversation == conversation)
            .map(|(id, op)| HistoryEntry {
                operation: id,
                generation: op.generation,
                accepted_frame: op.accepted_frame.sequence,
                terminal: None,
            });
        let below_generation = match before {
            None => {
                entries.extend(active);
                found.generation.saturating_add(1)
            }
            Some(sequence) => {
                let anchor = self.read(|state, store| {
                    state.entry(&Key::new(Tag::Activity, project, sequence), store)
                })?;
                match anchor {
                    Some(Entry::Activity {
                        conversation: c,
                        generation,
                        ..
                    }) if c == conversation => generation,
                    _ => return Err(Error::Invalid),
                }
            }
        };
        let lo = Key::new(Tag::Turn, conversation, 0);
        let hi = Key::new(Tag::Turn, conversation, below_generation);
        let want = limit + 1 - entries.len().min(limit);
        let turns = self.read(|state, store| state.range(&lo, &hi, true, want, store))?;
        for (key, value) in turns {
            let Entry::Turn {
                operation,
                terminal,
                accepted,
                ..
            } = Entry::decode(Tag::Turn, &value).map_err(lookup)?
            else {
                return Err(Error::RepairRequired);
            };
            if entries
                .iter()
                .any(|e: &HistoryEntry| e.operation == operation)
            {
                continue;
            }
            entries.push(HistoryEntry {
                operation,
                generation: key.disc,
                accepted_frame: accepted.sequence,
                terminal: Some(terminal),
            });
        }
        let has_more = entries.len() > limit;
        entries.truncate(limit);
        Ok((entries, has_more, Some(found.generation)))
    }
    /// The journal facts of one retained job payload: an unsettled job from
    /// the live state, or a settled one from its `X` entry, with the pin
    /// and delete state of its `E` entries.
    fn payload_fact(&mut self, payload: Id) -> Result<Option<PayloadFact>> {
        let state = self.state()?.clone();
        // An unsettled job (live state or checkpoint) is still live.
        if let Some(job) = state
            .jobs
            .values()
            .find(|job| job.payloads.contains(&payload))
        {
            let retention =
                self.read(|state, store| state.job_retention_summary(job.job, store))?;
            return Ok(Some(job_fact(
                payload,
                job.origin,
                job.job,
                job.payloads,
                job.intent,
                None,
                retention,
            )?));
        }
        let entry =
            self.read(|state, store| state.entry(&Key::new(Tag::Payload, payload, 0), store))?;
        // Form 2: a settled job, from the delta or the runs.
        if let Some(Entry::JobPayload {
            job,
            intent,
            settled,
            ..
        }) = entry
        {
            let Record::JobIntent(p) = self.record(intent)? else {
                return Err(Error::RepairRequired);
            };
            let Record::JobSettled(settlement) = self.record(settled)? else {
                return Err(Error::RepairRequired);
            };
            let payloads = job_payload_ids(state.installation_id, job);
            if job_id(&p.origin) != job || settlement.job != job || !payloads.contains(&payload) {
                return Err(Error::RepairRequired);
            }
            let retention = self.read(|state, store| state.job_retention_summary(job, store))?;
            // `interrupted` maps to no settlement: an unverified prefix, as
            // for a missing kind 28 (TJ-Q5).
            let settled = settlement.capture_state.capture().map(|capture| {
                (
                    capture,
                    settled.sequence,
                    [&settlement.stdout, &settlement.stderr]
                        .map(|s| (s.capture.stored_bytes, s.capture.digest)),
                )
            });
            let mut fact = job_fact(payload, p.origin, job, payloads, intent, settled, retention)?;
            fact.active = false;
            return Ok(Some(fact));
        }
        match entry {
            None => Ok(None),
            Some(_) => Err(Error::RepairRequired),
        }
    }
}
/// The `PayloadFacts` job form: `call` is the job ID, `operation` the model
/// origin's operation, `intent_sequence` the kind 26 sequence, `settled`
/// the kind 28 capture facts and the pin and delete state. An unsettled job
/// is `active`.
fn job_fact(
    payload: Id,
    origin: JobOrigin,
    job: Id,
    payloads: [Id; 2],
    intent: Locator,
    settled: Option<Settlement>,
    retention: JobRetentionSummary,
) -> Result<PayloadFact> {
    // Person origin replays only after P2.
    let JobOrigin::Model { operation, .. } = origin else {
        return Err(Error::RepairRequired);
    };
    Ok(PayloadFact {
        payload,
        call: job,
        operation,
        payloads,
        intent_sequence: intent.sequence,
        settled,
        active: true,
        pinned: retention.pinned,
        deleted: retention.deleted,
    })
}
