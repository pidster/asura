use super::*;
use crate::authority::{self, HEADER, MAX_FRAME, MAX_FRAMES, MAX_JOURNAL, ReplayError, TRAILER};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::sync::atomic::AtomicBool;

type Result<T> = std::result::Result<T, ReplayError>;
const TERMINAL_CAPACITY: usize = HEADER + TRAILER + 43 + MAX_TEXT;
const START_CAPACITY: usize = HEADER + TRAILER + 72;
const CANCEL_CAPACITY: usize = HEADER + TRAILER + 25;
const OWNER_CAPACITY: usize = HEADER + TRAILER;
/// One maximum kind 23 frame: 4,286 bytes.
const CAPTURE_CAPACITY: usize = HEADER + TRAILER + super::codec::MAX_CAPTURE_SETTLED_PAYLOAD;
const MAX_MEMORY: usize = 16 * 1024 * 1024;

#[derive(Debug)]
pub struct Replay {
    pub installation_id: Id,
    pub sequence: u64,
    pub end_offset: usize,
    pub last_digest: Hash,
    pub revision: u64,
    pub owner_generation: u64,
    pub initialization: PendingInit,
    pub binding: Option<ActiveBinding>,
    pub projects: BTreeMap<Id, ProjectRegistered>,
    pub project_names: BTreeMap<Id, ProjectName>,
    pub requests: BTreeMap<Id, RequestResult>,
    pub conversations: BTreeMap<Id, Conversation>,
    pub operations: BTreeMap<Id, Operation>,
    pub active_operation: Option<Id>,
    pub inputs: BTreeMap<Id, QueuedInput>,
    pub order_revision: u64,
    pub last_dispatched_lane: Option<Id>,
    pub reserved_bytes: usize,
    pub reserved_frames: usize,
    transition_ids: BTreeSet<Id>,
    generated_ids: BTreeSet<Id>,
    // Conservative per-entry allocation charges include B-tree nodes and strings.
    // Only bounded tool display prefixes are cloned; full bodies remain in frames.
    memory_charge: usize,
    input_charge: usize,
}
fn require(ok: bool) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(ReplayError::Corrupt)
    }
}
fn next(n: u64) -> Result<u64> {
    n.checked_add(1).ok_or(ReplayError::Limit)
}
/// UTF-8 byte bound independent of terminal rendering and journal encoding.
fn display_prefix(text: &str, limit: usize) -> String {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}
impl Replay {
    /// Return the effective display name and independent name revision.
    pub fn project_name(&self, project: Id) -> Option<(String, u64)> {
        let registered = self.projects.get(&project)?;
        if let Some(name) = self.project_names.get(&project) {
            return Some((name.name.clone(), name.revision));
        }
        let component = registered.location.rsplit('/').next().unwrap_or("");
        let default = capitalize_project_name(component).ok().unwrap_or_else(|| {
            let prefix: String = project[..4]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            format!("Project {prefix}")
        });
        Some((default, 0))
    }
    /// Resolve the last durably accepted conversation, independent of random IDs
    /// and terminal completion order.
    pub fn latest_conversation(&self, project: Id) -> Option<Id> {
        self.operations
            .values()
            .filter(|op| {
                self.conversations
                    .get(&op.conversation)
                    .is_some_and(|conversation| conversation.project == project)
            })
            .max_by_key(|op| op.accepted_frame.start)
            .map(|op| op.conversation)
    }
    /// Return an exclusive accepted-frame page in newest-first order. The
    /// caller requests one extra item to determine whether another page exists.
    pub fn accepted_page(
        &self,
        conversation: Id,
        before_accepted_frame: Option<usize>,
        limit: usize,
    ) -> Vec<(Id, &Operation)> {
        let mut entries: Vec<_> = self
            .operations
            .iter()
            .filter(|(_, op)| {
                op.conversation == conversation
                    && before_accepted_frame.is_none_or(|before| op.accepted_frame.start < before)
            })
            .map(|(id, op)| (*id, op))
            .collect();
        entries.sort_by_key(|(_, op)| std::cmp::Reverse(op.accepted_frame.start));
        entries.truncate(limit);
        entries
    }
    /// `run` create_note calls without kind 15, with their derived identity.
    pub fn unresolved_creates(&self) -> impl Iterator<Item = &CreateNoteCall> {
        self.operations
            .values()
            .flat_map(|op| op.tools.iter())
            .filter(|tool| tool.result_frame.is_none())
            .filter_map(|tool| tool.create.as_ref())
    }
    pub fn project_memory_blocked(&self, project: Id) -> bool {
        self.unresolved_creates()
            .any(|intent| intent.project == project)
    }
    fn charge(&mut self, bytes: usize) -> Result<()> {
        self.memory_charge = self
            .memory_charge
            .checked_add(bytes)
            .ok_or(ReplayError::Limit)?;
        if self.memory_charge + self.input_charge > MAX_MEMORY {
            return Err(ReplayError::Limit);
        }
        Ok(())
    }
    fn fresh(&mut self, id: Id) -> Result<()> {
        require(id != [0; 16] && self.generated_ids.insert(id))?;
        self.charge(128)
    }
    fn request(&mut self, id: Id, digest: Hash, result: Outcome) -> Result<()> {
        require(id != [0; 16] && !self.requests.contains_key(&id))?;
        require(
            !self.inputs.values().any(|q| q.dispatch_request == id)
                || matches!(result, Outcome::Turn { .. }),
        )?;
        self.charge(384)?;
        self.requests.insert(id, RequestResult { digest, result });
        Ok(())
    }
    fn operation_mut(&mut self, id: Id, generation: u64) -> Result<&mut Operation> {
        require(self.active_operation == Some(id))?;
        let operation = self.operations.get_mut(&id).ok_or(ReplayError::Corrupt)?;
        require(operation.generation == generation && operation.terminal.is_none())?;
        Ok(operation)
    }
    fn apply(&mut self, record: Record, generation: u64, frame: Range<usize>) -> Result<()> {
        if !matches!(record, Record::OwnerGeneration) {
            require(generation == self.owner_generation)?;
        }
        if let Some(id) = self.active_operation {
            let old = self
                .operations
                .get(&id)
                .ok_or(ReplayError::Corrupt)?
                .owner_generation
                != self.owner_generation;
            if old {
                let reconciliation = match &record {
                    Record::ToolResult(result) => self.operations.get(&id).is_some_and(|op| {
                        result.operation == id
                            && result.generation == op.generation
                            && result.ordinal as usize == op.tools.len()
                            && op.tools.last().is_some_and(|tool| {
                                tool.create.is_some() && tool.result_frame.is_none()
                            })
                    }),
                    _ => false,
                };
                require(
                    reconciliation
                        || matches!(record, Record::OwnerGeneration | Record::TurnTerminal(_)),
                )?;
            }
        }
        match record {
            Record::PendingInit(_) => return Err(ReplayError::Corrupt),
            Record::ActiveBinding(p) => {
                require(
                    self.binding.is_none()
                        && p.request == self.initialization.request
                        && p.generation == 1
                        && p.graph == self.initialization.graph
                        && p.configuration_digest == self.initialization.configuration_digest,
                )?;
                self.requests
                    .get_mut(&p.request)
                    .ok_or(ReplayError::Corrupt)?
                    .result = Outcome::Initialization { active: true };
                self.binding = Some(p);
            }
            Record::OwnerGeneration => {
                require(generation == next(self.owner_generation)?)?;
                self.owner_generation = generation;
                for input in self.inputs.values_mut() {
                    if !self.requests.contains_key(&input.dispatch_request) && !input.dropped {
                        input.held = true;
                        input.resumed = false;
                    }
                }
            }
            Record::ProjectRegistered(p) => {
                require(self.binding.is_some() && p.registry_revision == 1 && p.visibility == 1)?;
                require(
                    p.digest
                        == request_digest(Request::Register {
                            location: &p.location,
                        })?,
                )?;
                require(!self.projects.values().any(|q| {
                    q.location == p.location || (q.device == p.device && q.inode == p.inode)
                }))?;
                if self.projects.len() >= MAX_PROJECTS {
                    return Err(ReplayError::Limit);
                }
                self.fresh(p.project)?;
                self.request(p.request, p.digest, Outcome::Project(p.project))?;
                self.charge(512 + p.location.len())?;
                self.projects.insert(p.project, p);
            }
            Record::ProjectRequestAlias(p) => {
                require(self.binding.is_some())?;
                let project = self.projects.get(&p.project).ok_or(ReplayError::Corrupt)?;
                require(
                    p.registry_revision == project.registry_revision
                        && p.digest
                            == request_digest(Request::Register {
                                location: &p.location,
                            })?,
                )?;
                self.request(p.request, p.digest, Outcome::Project(p.project))?;
            }
            Record::ProjectRenamed(p) => {
                require(self.binding.is_some() && self.projects.contains_key(&p.project))?;
                require(
                    p.digest
                        == request_digest(Request::RenameProject {
                            project: p.project,
                            expected_name_revision: p.expected_name_revision,
                            name: &p.requested_name,
                        })?
                        && p.name == capitalize_project_name(&p.requested_name)?,
                )?;
                let (prior_name, prior_revision) =
                    self.project_name(p.project).ok_or(ReplayError::Corrupt)?;
                require(
                    p.expected_name_revision == prior_revision
                        && p.name_revision == next(prior_revision)?,
                )?;
                let changed = prior_name != p.name;
                self.request(
                    p.request,
                    p.digest,
                    Outcome::ProjectRename {
                        project: p.project,
                        name: p.name.clone(),
                        name_revision: p.name_revision,
                        changed,
                    },
                )?;
                self.charge(256 + p.name.len())?;
                self.project_names.insert(
                    p.project,
                    ProjectName {
                        name: p.name,
                        revision: p.name_revision,
                    },
                );
            }
            Record::InputQueued(p) => self.enqueue(p, generation, frame)?,
            Record::InputReordered(p) => self.reorder_input(p)?,
            Record::InputPromoted(p) => {
                require(
                    p.digest
                        == request_digest(Request::PromoteInput {
                            input: p.input,
                            target_operation: p.target_operation,
                            target_generation: p.target_generation,
                        })?,
                )?;
                require(self.can_promote_input(p.input, p.target_operation, p.target_generation))?;
                self.request(p.request, p.digest, Outcome::InputDecision(p.input))?;
                let input = self.inputs.get_mut(&p.input).ok_or(ReplayError::Corrupt)?;
                input.kind = InputKind::Steer;
                input.held = false;
                input.target_operation = p.target_operation;
                input.target_generation = p.target_generation;
                input.resumed = false;
                input.owner_generation = generation;
                self.order_revision = next(self.order_revision)?;
            }
            Record::InputDecision(p) => {
                require(
                    p.digest
                        == request_digest(Request::InputDecision {
                            input: p.input,
                            action: p.action,
                        })?,
                )?;
                let status = self.input_status(p.input).ok_or(ReplayError::Corrupt)?;
                require(if p.action == InputAction::Hold {
                    status == InputStatus::Queued
                } else {
                    status == InputStatus::Held
                })?;
                self.request(p.request, p.digest, Outcome::InputDecision(p.input))?;
                let input = self.inputs.get_mut(&p.input).ok_or(ReplayError::Corrupt)?;
                match p.action {
                    InputAction::Resume => {
                        input.held = false;
                        input.resumed = true;
                        input.owner_generation = generation;
                    }
                    InputAction::Drop => input.dropped = true,
                    InputAction::Hold => input.held = true,
                }
                self.order_revision = next(self.order_revision)?;
            }
            Record::TurnAccepted(p) => self.accept(*p, generation, frame)?,
            Record::StartAuthorized(p) => {
                let owner = self.owner_generation;
                let op = self.operation_mut(p.operation, p.generation)?;
                require(
                    op.owner_generation == owner
                        && op.helper.is_none()
                        && op.cancel.is_none()
                        && op.input_digest == p.input_digest
                        && p.helper != [0; 16],
                )?;
                op.helper = Some(p.helper);
            }
            Record::CancelRequested(p) => {
                require(matches!(
                    p.cause,
                    Cause::UserCancel
                        | Cause::Steering
                        | Cause::Deadline
                        | Cause::OutputLimit
                        | Cause::ServiceShutdown
                ))?;
                let op = self.operation_mut(p.operation, p.generation)?;
                require(op.cancel.is_none())?;
                op.cancel = Some(p.cause);
            }
            Record::ToolCall(p) => self.tool_call(*p, frame)?,
            Record::CaptureSettled(p) => self.capture_settled(*p, frame)?,
            Record::ToolResult(p) => {
                let owner = self.owner_generation;
                let op = self.operation_mut(p.operation, p.generation)?;
                require(p.ordinal as usize == op.tools.len())?;
                require(
                    op.owner_generation == owner
                        || op.tools.last().is_some_and(|tool| {
                            tool.create.is_some() && tool.result_frame.is_none()
                        }),
                )?;
                // No turn result-byte total (owner, 2026-10-18).
                let tool = op.tools.last_mut().ok_or(ReplayError::Corrupt)?;
                // Only a `run` call is followed by kind 15 (TCR-R5).
                require(tool.unresolved())?;
                let operation = tool.tool_operation.ok_or(ReplayError::Corrupt)?;
                match operation {
                    // J1 order: run 14, 23, 15.
                    ToolOperation::ExecCmd => require(
                        tool.exec_cmd
                            .as_deref()
                            .is_some_and(|call| call.settled.is_some())
                            && p.next_offset.is_none(),
                    )?,
                    // TCR-D1: the text is rendered from the derived identity.
                    ToolOperation::CreateNote => require(
                        p.next_offset.is_none()
                            && !p.truncated
                            && p.text.is_empty()
                            && matches!(p.status, 1 | 2 | 4 | 5 | 6 | 7),
                    )?,
                    // The structural codec cannot see the operation. exec_cmd is
                    // the sole exception for captured timeout/cancel/limit tails.
                    _ if p.status != 1 => require(p.next_offset.is_none() && !p.truncated)?,
                    // TCR-D25: no limit or offset comparison; replay never parses
                    // the stored arguments again.
                    ToolOperation::ReadFile | ToolOperation::GetNote => {
                        require(p.next_offset.is_some())?
                    }
                    ToolOperation::ListDirectory => require(p.next_offset.is_none())?,
                    _ => require(p.next_offset.is_none() && !p.truncated)?,
                }
                tool.result_frame = Some(frame);
                tool.result_status = Some(p.status);
                tool.result_bytes = p.text.len();
                tool.preview = Some(match operation {
                    ToolOperation::GetNote => format!("captured {} bytes", p.text.len()),
                    ToolOperation::CreateNote => String::new(),
                    ToolOperation::ExecCmd => {
                        let output = p
                            .text
                            .split_once("\nstdout:\n")
                            .map(|(_, body)| body)
                            .unwrap_or("");
                        let (stdout, stderr) =
                            output.split_once("\nstderr:\n").unwrap_or((output, ""));
                        display_prefix(if stdout.is_empty() { stderr } else { stdout }, 128)
                    }
                    _ => display_prefix(&p.text, 128),
                });
                tool.truncated = Some(p.truncated);
                if operation == ToolOperation::ExecCmd {
                    tool.exit_code = p
                        .text
                        .lines()
                        .next()
                        .and_then(|line| {
                            line.split_whitespace()
                                .find_map(|field| field.strip_prefix("exit_code="))
                        })
                        .and_then(|value| value.parse().ok());
                }
            }
            Record::TurnTerminal(p) => self.terminal(p, frame)?,
        }
        Ok(())
    }
    /// Kind 14 (tool-call-records.md, replay rules). `decode_record` has
    /// validated the record alone; replay adds the operation, ordinal and cancel
    /// rules and recomputes the derived identities (TCR-O5).
    fn tool_call(&mut self, p: ToolCall, frame: Range<usize>) -> Result<()> {
        let run = p.decision == ToolDecision::Run;
        let create = if run && p.tool_operation == Some(ToolOperation::CreateNote) {
            if self.unresolved_creates().count() >= 32 {
                return Err(ReplayError::Limit);
            }
            let op = self
                .operations
                .get(&p.operation)
                .ok_or(ReplayError::Corrupt)?;
            let project = self
                .conversations
                .get(&op.conversation)
                .ok_or(ReplayError::Corrupt)?
                .project;
            require(self.projects.contains_key(&project) && !self.project_memory_blocked(project))?;
            Some(CreateNoteCall::derive(
                [
                    self.installation_id,
                    self.initialization.graph,
                    self.initialization.request,
                ],
                project,
                p.operation,
                p.generation,
                p.ordinal,
            )?)
        } else {
            None
        };
        let sequence = next(self.sequence)?;
        let exec_cmd = (run && p.tool_operation == Some(ToolOperation::ExecCmd)).then(|| {
            let payload = |kind| {
                crate::payload::payload_id(
                    self.installation_id,
                    p.operation,
                    p.generation,
                    p.ordinal,
                    kind,
                )
            };
            Box::new(ExecCmdCall {
                job: exec_cmd_job_id(p.operation, p.generation, p.ordinal),
                stdout_payload: payload(crate::payload::ObjectKind::Stdout),
                stderr_payload: payload(crate::payload::ObjectKind::Stderr),
                intent_sequence: sequence,
                settled: None,
            })
        });
        // ToolRecord, derived identities, the name and the display prefix.
        self.charge(1536 + p.name.len())?;
        let owner = self.owner_generation;
        let op = self.operation_mut(p.operation, p.generation)?;
        require(
            op.owner_generation == owner
                && op.helper.is_some()
                && p.ordinal as usize == op.tools.len() + 1
                && op.tools.iter().all(|t| !t.unresolved())
                && (op.cancel.is_none() || p.decision == ToolDecision::Cancelled),
        )?;
        op.tools.push(ToolRecord {
            tool: RegisteredTool::lookup(&p.name),
            arguments: display_prefix(&p.arguments, 192),
            name: p.name,
            tool_operation: p.tool_operation,
            decision: p.decision,
            fault: p.fault,
            busy: p.busy,
            refusal: p.refusal,
            form: p.form,
            arguments_bytes: p.arguments_bytes,
            result_status: None,
            intent_frame: frame,
            result_frame: None,
            result_bytes: 0,
            preview: None,
            truncated: None,
            exit_code: None,
            create,
            exec_cmd,
        });
        Ok(())
    }
    /// Kind 23 settles the pending `run` exec_cmd kind 14, once, before kind 15.
    fn capture_settled(&mut self, p: CaptureSettled, frame: Range<usize>) -> Result<()> {
        p.validate()?;
        self.charge(256)?;
        let sequence = next(self.sequence)?;
        let owner = self.owner_generation;
        let op = self.operation_mut(p.operation, p.generation)?;
        require(op.owner_generation == owner && p.ordinal as usize == op.tools.len())?;
        let tool = op.tools.last_mut().ok_or(ReplayError::Corrupt)?;
        require(tool.result_frame.is_none())?;
        let exec_cmd = tool.exec_cmd.as_deref_mut().ok_or(ReplayError::Corrupt)?;
        require(exec_cmd.settled.is_none())?;
        exec_cmd.settled = Some(ExecCmdSettlement {
            state: p.state,
            reason: p.reason,
            sequence,
            streams: [
                (p.stdout.stored_bytes, p.stdout.digest),
                (p.stderr.stored_bytes, p.stderr.digest),
            ],
            frame,
        });
        Ok(())
    }
    fn accept(&mut self, p: TurnAccepted, owner: u64, frame: Range<usize>) -> Result<()> {
        require(
            self.binding.is_some()
                && self.active_operation.is_none()
                && self.projects.contains_key(&p.project),
        )?;
        if let Some(input) = self
            .inputs
            .values()
            .find(|q| q.dispatch_request == p.request)
        {
            require(
                input.project == p.project
                    && Some(input.conversation) == p.original_conversation
                    && self.input_ready(input.request),
            )?;
        }
        require(p.event_cursor == 1 && !p.model.is_empty())?;
        require(
            p.digest
                == request_digest(Request::Submit {
                    project: p.project,
                    conversation: p.original_conversation,
                    expected_generation: p.expected_generation,
                    prompt: &p.prompt,
                })?,
        )?;
        match p.original_conversation {
            None => {
                require(p.generation == 1)?;
                if self.conversations.len() >= MAX_CONVERSATIONS {
                    return Err(ReplayError::Limit);
                }
                self.fresh(p.conversation)?;
                self.charge(256)?;
            }
            Some(id) => {
                let c = self.conversations.get(&id).ok_or(ReplayError::Corrupt)?;
                require(
                    id == p.conversation
                        && c.project == p.project
                        && c.generation == p.expected_generation
                        && p.generation == next(c.generation)?,
                )?;
                if c.generation == 0 {
                    require(
                        self.inputs
                            .values()
                            .any(|q| q.conversation == id && q.dispatch_request == p.request),
                    )?;
                }
            }
        }
        let mut prior = BTreeSet::new();
        let mut last_generation = 0;
        for id in &p.prior_operations {
            let op = self.operations.get(id).ok_or(ReplayError::Corrupt)?;
            require(
                prior.insert(*id)
                    && op.conversation == p.conversation
                    && op.generation > last_generation
                    && op
                        .terminal
                        .as_ref()
                        .is_some_and(|t| t.kind == TerminalKind::Complete),
            )?;
            last_generation = op.generation;
        }
        // Every turn accounts for every completed prior turn: shown or omitted.
        let completed = self
            .operations
            .values()
            .filter(|op| {
                op.conversation == p.conversation
                    && op.generation < p.generation
                    && op
                        .terminal
                        .as_ref()
                        .is_some_and(|t| t.kind == TerminalKind::Complete)
            })
            .count();
        require(p.prior_operations.len() + p.composition.omitted_turns as usize == completed)?;
        self.fresh(p.task)?;
        self.fresh(p.operation)?;
        self.request(
            p.request,
            p.digest,
            Outcome::Turn {
                conversation: p.conversation,
                generation: p.generation,
                task: p.task,
                operation: p.operation,
            },
        )?;
        self.charge(512)?;
        self.conversations.insert(
            p.conversation,
            Conversation {
                project: p.project,
                generation: p.generation,
            },
        );
        self.operations.insert(
            p.operation,
            Operation {
                conversation: p.conversation,
                generation: p.generation,
                task: p.task,
                owner_generation: owner,
                input_digest: p.input_digest,
                accepted_frame: frame,
                helper: None,
                cancel: None,
                terminal: None,
                tools: Vec::new(),
            },
        );
        self.active_operation = Some(p.operation);
        self.last_dispatched_lane = Some(p.conversation);
        if let Some(input) = self
            .inputs
            .values()
            .find(|q| q.dispatch_request == p.request)
        {
            let _ = input;
            self.order_revision = next(self.order_revision)?;
        }
        Ok(())
    }
    fn enqueue(&mut self, p: InputQueued, owner: u64, frame: Range<usize>) -> Result<()> {
        require(self.binding.is_some() && self.projects.contains_key(&p.project))?;
        require(
            p.input != [0; 16]
                && p.input != p.request
                && p.input != p.dispatch_request
                && p.dispatch_request != p.request,
        )?;
        require(p.order_revision == next(self.order_revision)?)?;
        let original = if p.new_conversation {
            None
        } else {
            Some(p.conversation)
        };
        require(
            p.digest
                == request_digest(Request::QueueInput {
                    project: p.project,
                    conversation: original,
                    expected_generation: p.expected_generation,
                    new_conversation: p.new_conversation,
                    prompt: &p.prompt,
                })?,
        )?;
        if p.new_conversation {
            require(
                p.expected_generation == 0 && !self.conversations.contains_key(&p.conversation),
            )?;
            require(!self.inputs.values().any(|q| {
                q.project == p.project
                    && self
                        .conversations
                        .get(&q.conversation)
                        .is_some_and(|c| c.generation == 0)
                    && matches!(
                        self.input_status(q.request),
                        Some(InputStatus::Queued | InputStatus::Held)
                    )
            }))?;
            if self.conversations.len() >= MAX_CONVERSATIONS {
                return Err(ReplayError::Limit);
            }
        } else {
            require(
                self.conversations.get(&p.conversation).is_some_and(|c| {
                    c.project == p.project && p.expected_generation <= c.generation
                }),
            )?;
        }
        if self
            .inputs
            .values()
            .filter(|q| {
                matches!(
                    self.input_status(q.request),
                    Some(InputStatus::Queued | InputStatus::Held | InputStatus::Running)
                )
            })
            .count()
            >= MAX_INPUTS
        {
            return Err(ReplayError::Limit);
        }
        require(
            !self.inputs.contains_key(&p.input)
                && !self.requests.contains_key(&p.dispatch_request)
                && !self
                    .inputs
                    .values()
                    .any(|q| q.dispatch_request == p.dispatch_request),
        )?;
        self.fresh(p.input)?;
        self.fresh(p.dispatch_request)?;
        if p.new_conversation {
            self.fresh(p.conversation)?;
            self.charge(256)?;
            self.conversations.insert(
                p.conversation,
                Conversation {
                    project: p.project,
                    generation: 0,
                },
            );
        }
        self.request(
            p.request,
            p.digest,
            Outcome::QueueInput {
                input: p.input,
                conversation: p.conversation,
                order_revision: p.order_revision,
            },
        )?;
        self.charge(512)?;
        let order_position = self
            .inputs
            .values()
            .filter(|q| q.conversation == p.conversation)
            .map(|q| q.order_position)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(ReplayError::Limit)?;
        self.inputs.insert(
            p.input,
            QueuedInput {
                request: p.input,
                dispatch_request: p.dispatch_request,
                project: p.project,
                conversation: p.conversation,
                target_operation: [0; 16],
                target_generation: p.expected_generation,
                kind: InputKind::Queue,
                frame,
                sequence: next(self.sequence)?,
                owner_generation: owner,
                held: false,
                dropped: false,
                resumed: false,
                order_position,
            },
        );
        self.order_revision = p.order_revision;
        Ok(())
    }
    fn reorder_input(&mut self, p: InputReordered) -> Result<()> {
        require(
            p.expected_order_revision == self.order_revision
                && p.order_revision == next(self.order_revision)?,
        )?;
        require(
            p.digest
                == request_digest(Request::ReorderInput {
                    input: p.input,
                    after: p.after,
                    expected_order_revision: p.expected_order_revision,
                })?,
        )?;
        let input = self.inputs.get(&p.input).ok_or(ReplayError::Corrupt)?;
        require(matches!(
            self.input_status(p.input),
            Some(InputStatus::Queued | InputStatus::Held)
        ))?;
        let lane = input.conversation;
        if let Some(after) = p.after {
            let anchor = self.inputs.get(&after).ok_or(ReplayError::Corrupt)?;
            require(
                anchor.conversation == lane
                    && matches!(
                        self.input_status(after),
                        Some(InputStatus::Queued | InputStatus::Held)
                    ),
            )?;
        }
        let mut ids: Vec<_> = self
            .inputs
            .values()
            .filter(|q| {
                q.conversation == lane
                    && matches!(
                        self.input_status(q.request),
                        Some(InputStatus::Queued | InputStatus::Held)
                    )
            })
            .map(|q| (q.order_position, q.request))
            .collect();
        ids.sort();
        let mut ids: Vec<_> = ids.into_iter().map(|(_, id)| id).collect();
        ids.retain(|id| *id != p.input);
        let position = p.after.map_or(0, |after| {
            ids.iter()
                .position(|id| *id == after)
                .map(|n| n + 1)
                .unwrap_or(usize::MAX)
        });
        require(position <= ids.len())?;
        ids.insert(position, p.input);
        self.request(
            p.request,
            p.digest,
            Outcome::ReorderInput {
                input: p.input,
                order_revision: p.order_revision,
            },
        )?;
        for (index, id) in ids.into_iter().enumerate() {
            self.inputs
                .get_mut(&id)
                .ok_or(ReplayError::Corrupt)?
                .order_position = (index + 1) as u64;
        }
        self.order_revision = p.order_revision;
        Ok(())
    }
    pub fn can_promote_input(&self, input: Id, target: Id, generation: u64) -> bool {
        let Some(input) = self.inputs.get(&input) else {
            return false;
        };
        input.kind == InputKind::Queue
            && matches!(
                self.input_status(input.request),
                Some(InputStatus::Queued | InputStatus::Held)
            )
            && self.input_operation(input.request).is_none()
            && self.active_operation == Some(target)
            && self.operations.get(&target).is_some_and(|op| {
                op.conversation == input.conversation
                    && op.generation == generation
                    && op.terminal.is_none()
                    && op.cancel.is_none()
            })
            && self.steering_input(target).is_none()
    }
    pub fn input_operation(&self, id: Id) -> Option<(Id, u64)> {
        let input = self.inputs.get(&id)?;
        match self.requests.get(&input.dispatch_request)?.result {
            Outcome::Turn {
                operation,
                generation,
                ..
            } => Some((operation, generation)),
            _ => None,
        }
    }
    pub fn input_status(&self, id: Id) -> Option<InputStatus> {
        let input = self.inputs.get(&id)?;
        if let Some((operation, _)) = self.input_operation(id) {
            let op = self.operations.get(&operation)?;
            return Some(match &op.terminal {
                None => InputStatus::Running,
                Some(t) if t.kind == TerminalKind::Complete => InputStatus::Complete,
                Some(_) => InputStatus::Failed,
            });
        }
        if input.dropped {
            return Some(InputStatus::Dropped);
        }
        if input.held {
            return Some(InputStatus::Held);
        }
        if input.resumed {
            return Some(InputStatus::Queued);
        }
        if input.kind == InputKind::Steer {
            let target = self.operations.get(&input.target_operation)?;
            return Some(match &target.terminal {
                None => InputStatus::Queued,
                Some(t)
                    if t.kind == TerminalKind::Cancelled
                        && t.cause == Cause::Steering
                        && input.owner_generation == self.owner_generation =>
                {
                    InputStatus::Queued
                }
                Some(_) => InputStatus::Held,
            });
        }
        let latest = self
            .operations
            .values()
            .filter(|op| op.conversation == input.conversation)
            .max_by_key(|op| op.generation);
        Some(match latest.and_then(|op| op.terminal.as_ref()) {
            // A committed failure or cancellation settles the old turn.
            // Owner restart and explicit holds remain separate recovery gates.
            Some(t) if t.kind == TerminalKind::Interrupted => InputStatus::Held,
            _ => InputStatus::Queued,
        })
    }
    pub fn input_ready(&self, id: Id) -> bool {
        let Some(input) = self.inputs.get(&id) else {
            return false;
        };
        if self.input_status(id) != Some(InputStatus::Queued) {
            return false;
        }
        if input.kind == InputKind::Steer {
            return self
                .operations
                .get(&input.target_operation)
                .is_some_and(|op| op.terminal.is_some());
        }
        !self.inputs.values().any(|prior| {
            prior.conversation == input.conversation
                && prior.request != id
                && matches!(
                    self.input_status(prior.request),
                    Some(InputStatus::Queued | InputStatus::Held | InputStatus::Running)
                )
                && prior.order_position < input.order_position
        })
    }
    pub fn ready_input(&self) -> Option<Id> {
        if self.active_operation.is_some() {
            return None;
        }
        let mut lanes: BTreeMap<Id, (&QueuedInput, u64)> = BTreeMap::new();
        for input in self.inputs.values().filter(|q| self.input_ready(q.request)) {
            let lane_sequence = self
                .inputs
                .values()
                .filter(|q| q.conversation == input.conversation)
                .map(|q| q.sequence)
                .min()
                .unwrap_or(input.sequence);
            lanes
                .entry(input.conversation)
                .and_modify(|(current, _)| {
                    if (input.kind != InputKind::Steer, input.order_position)
                        < (current.kind != InputKind::Steer, current.order_position)
                    {
                        *current = input;
                    }
                })
                .or_insert((input, lane_sequence));
        }
        let mut lanes: Vec<_> = lanes.into_values().collect();
        lanes.sort_by_key(|(_, lane_sequence)| *lane_sequence);
        let position = self.last_dispatched_lane.and_then(|lane| {
            lanes
                .iter()
                .position(|(input, _)| input.conversation == lane)
        });
        let pick = position.map_or(0, |index| (index + 1) % lanes.len());
        lanes.get(pick).map(|(input, _)| input.request)
    }
    pub fn steering_input(&self, operation: Id) -> Option<Id> {
        self.inputs
            .values()
            .find(|input| {
                input.kind == InputKind::Steer
                    && input.target_operation == operation
                    && self.input_status(input.request) == Some(InputStatus::Queued)
            })
            .map(|input| input.request)
    }
    fn terminal(&mut self, p: TurnTerminal, frame: Range<usize>) -> Result<()> {
        let owner = self.owner_generation;
        let op = self.operation_mut(p.operation, p.generation)?;
        require(p.final_cursor == u64::MAX)?;
        require(
            op.tools
                .iter()
                .all(|tool| tool.create.is_none() || tool.result_frame.is_some()),
        )?;
        // No turn token reservation exists (owner, 2026-10-18): known usage
        // is any count; unknown usage records zero.
        if !p.usage_known {
            require(p.output_tokens == 0)?;
        }
        if op.helper.is_none() {
            require(p.usage_known && p.output_tokens == 0)?;
        }
        match p.kind {
            TerminalKind::Complete => require(
                // Every `run` call has its kind 15; a call that does not run is
                // complete at kind 14 (TCR-R5).
                op.helper.is_some()
                    && op.tools.iter().all(|t| !t.unresolved())
                    && op.cancel.is_none()
                    && p.cause == Cause::None
                    && !p.text.is_empty()
                    && op.owner_generation == owner,
            )?,
            TerminalKind::Interrupted => {
                require(p.cause == Cause::Restart && op.owner_generation < owner)?;
                if op.helper.is_some() {
                    require(!p.usage_known)?;
                }
            }
            TerminalKind::Cancelled => require(
                op.cancel.is_some()
                    && p.cause == op.cancel.unwrap()
                    && op.owner_generation == owner,
            )?,
            TerminalKind::Failed => require(
                p.cause != Cause::None && p.cause != Cause::Restart && op.owner_generation == owner,
            )?,
        }
        op.terminal = Some(Terminal {
            kind: p.kind,
            cause: p.cause,
            usage_known: p.usage_known,
            output_tokens: p.output_tokens,
            frame,
        });
        self.active_operation = None;
        Ok(())
    }
    fn reserve(&mut self) -> Result<()> {
        self.reserved_bytes = 0;
        self.reserved_frames = 0;
        if let Some(id) = self.active_operation {
            let op = self.operations.get(&id).ok_or(ReplayError::Corrupt)?;
            self.reserved_bytes = TERMINAL_CAPACITY;
            self.reserved_frames = 1;
            // A `run` kind 14 holds its kind 15 and, for exec_cmd, its kind 23
            // (TCR-D16). A call that does not run holds nothing more.
            if let Some(tool) = op.tools.last().filter(|tool| tool.unresolved()) {
                self.reserved_bytes += HEADER + TRAILER + 43 + 16_384;
                self.reserved_frames += 1;
                if tool
                    .exec_cmd
                    .as_deref()
                    .is_some_and(|exec_cmd| exec_cmd.settled.is_none())
                {
                    self.reserved_bytes += CAPTURE_CAPACITY;
                    self.reserved_frames += 1;
                }
            }
            if op.owner_generation == self.owner_generation {
                self.reserved_bytes += OWNER_CAPACITY;
                self.reserved_frames += 1;
                if op.helper.is_none() && op.cancel.is_none() {
                    self.reserved_bytes += START_CAPACITY;
                    self.reserved_frames += 1;
                }
                if op.cancel.is_none() {
                    self.reserved_bytes += CANCEL_CAPACITY;
                    self.reserved_frames += 1;
                }
            }
        }
        for input in self.inputs.values() {
            let frames = match self.input_status(input.request) {
                Some(InputStatus::Queued) => 2,
                Some(InputStatus::Held) => 1,
                _ => 0,
            };
            self.reserved_frames += frames;
            self.reserved_bytes += frames * (HEADER + TRAILER + 65);
        }
        if self.end_offset + self.reserved_bytes > MAX_JOURNAL
            || self.sequence as usize + self.reserved_frames > MAX_FRAMES
            || self.memory_charge
                + self.end_offset
                + self.reserved_bytes
                + self.reserved_frames * 128
                > MAX_MEMORY
        {
            return Err(ReplayError::Limit);
        }
        Ok(())
    }
}
/// Strict all-or-nothing replay. Returned offsets refer only to the input journal;
/// the caller must retain its validated immutable identity before reading them.
pub fn replay(bytes: &[u8], cancelled: &AtomicBool) -> Result<Replay> {
    authority::active(cancelled)?;
    if bytes.len() > MAX_JOURNAL {
        return Err(ReplayError::Limit);
    }
    if bytes.is_empty() {
        return Err(ReplayError::Empty);
    }
    let mut result: Option<Replay> = None;
    let mut offset = 0;
    while offset < bytes.len() {
        authority::active(cancelled)?;
        let envelope = authority::envelope(&bytes[offset..])?;
        let ctx = &envelope.context;
        let record = decode_record(envelope.kind, envelope.payload)?;
        let end = offset + envelope.length;
        if let Some(s) = &mut result {
            require(
                ctx.sequence == next(s.sequence)?
                    && ctx.prior_digest == s.last_digest
                    && ctx.expected_revision == s.revision
                    && ctx.installation_id == s.installation_id
                    && s.transition_ids.insert(ctx.transition_id),
            )?;
            if ctx.sequence > MAX_FRAMES as u64 {
                return Err(ReplayError::Limit);
            }
            s.charge(128)?;
            s.apply(record, ctx.owner_generation, offset..end)?;
        } else {
            require(
                ctx.sequence == 1
                    && ctx.expected_revision == 0
                    && ctx.owner_generation == 1
                    && ctx.prior_digest == [0; 32],
            )?;
            let Record::PendingInit(p) = record else {
                return Err(ReplayError::Corrupt);
            };
            require(
                p.configuration_revision > 0
                    && p.graph != [0; 16]
                    && p.digest == request_digest(Request::Initialize { mode: p.mode })?,
            )?;
            let mut s = Replay {
                installation_id: ctx.installation_id,
                sequence: 0,
                end_offset: 0,
                last_digest: [0; 32],
                revision: 0,
                owner_generation: 1,
                initialization: p.clone(),
                binding: None,
                projects: BTreeMap::new(),
                project_names: BTreeMap::new(),
                requests: BTreeMap::new(),
                conversations: BTreeMap::new(),
                operations: BTreeMap::new(),
                active_operation: None,
                inputs: BTreeMap::new(),
                order_revision: 0,
                last_dispatched_lane: None,
                reserved_bytes: 0,
                reserved_frames: 0,
                transition_ids: BTreeSet::from([ctx.transition_id]),
                generated_ids: BTreeSet::new(),
                memory_charge: MAX_FRAME + 4096,
                input_charge: bytes.len(),
            };
            s.fresh(ctx.installation_id)?;
            s.fresh(p.graph)?;
            s.request(
                p.request,
                p.digest,
                Outcome::Initialization { active: false },
            )?;
            result = Some(s);
        }
        let s = result.as_mut().ok_or(ReplayError::Corrupt)?;
        s.sequence = ctx.sequence;
        s.end_offset = end;
        s.revision = next(ctx.expected_revision)?;
        s.last_digest = envelope.digest;
        s.reserve()?;
        offset = end;
    }
    authority::active(cancelled)?;
    result.ok_or(ReplayError::Empty)
}

#[cfg(test)]
mod restoration_tests {
    use super::*;

    #[test]
    fn latest_conversation_uses_accepted_frame_and_project() {
        let frame = encode_frame(
            &FrameContext {
                installation_id: [1; 16],
                transition_id: [2; 16],
                sequence: 1,
                prior_digest: [0; 32],
                expected_revision: 0,
                owner_generation: 1,
            },
            &Record::PendingInit(PendingInit {
                request: [3; 16],
                digest: request_digest(Request::Initialize { mode: 1 }).unwrap(),
                mode: 1,
                configuration_revision: 1,
                configuration_digest: [4; 32],
                graph: [5; 16],
            }),
        )
        .unwrap();
        let mut state = replay(&frame, &AtomicBool::new(false)).unwrap();
        state.conversations.insert(
            [9; 16],
            Conversation {
                project: [6; 16],
                generation: 1,
            },
        );
        state.conversations.insert(
            [7; 16],
            Conversation {
                project: [6; 16],
                generation: 4,
            },
        );
        state.conversations.insert(
            [8; 16],
            Conversation {
                project: [10; 16],
                generation: 1,
            },
        );
        let op = |conversation, frame| Operation {
            conversation,
            generation: 1,
            task: [11; 16],
            owner_generation: 1,
            input_digest: [12; 32],
            accepted_frame: frame..frame + 1,
            helper: None,
            cancel: None,
            terminal: None,
            tools: Vec::new(),
        };
        // ID order is opposite acceptance order, and a later other-project
        // admission cannot displace this project's selected conversation.
        state.operations.insert([2; 16], op([9; 16], 200));
        state.operations.insert([3; 16], op([7; 16], 300));
        state.operations.insert([4; 16], op([8; 16], 400));
        assert_eq!(state.latest_conversation([6; 16]), Some([7; 16]));
        assert_eq!(state.latest_conversation([10; 16]), Some([8; 16]));
        assert_eq!(state.latest_conversation([13; 16]), None);
        state.operations.insert([5; 16], op([7; 16], 250));
        let first = state.accepted_page([7; 16], None, 1);
        assert_eq!(
            first.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            vec![[3; 16]]
        );
        let second = state.accepted_page([7; 16], Some(300), 2);
        assert_eq!(
            second.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            vec![[5; 16]]
        );
    }
}
