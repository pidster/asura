//! Checkpoint files: the live state at one frame sequence `S`, the index
//! runs that cover frames up to `S`, and the segment census. A checkpoint is
//! a projection of frames `1..S`; it holds stored facts only. Replay
//! recomputes every derived value after loading it.
use super::ReplayError;
use super::conversation::*;
use sha2::{Digest, Sha256};

type Result<T> = std::result::Result<T, ReplayError>;
pub const MAX_CHECKPOINT: usize = 6 * 1024 * 1024;
/// Index runs one checkpoint may list.
pub const MAX_RUNS: usize = 64;
const MAGIC: &[u8; 8] = b"ASURAK01";
const COMMIT: &[u8; 8] = b"ASURAC01";
const MAX_LOCATION: usize = 4096;
const MAX_NAME: usize = 512;

/// One index run a checkpoint lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunRef {
    pub first: u64,
    pub last: u64,
    pub level: u8,
    pub entries: u64,
    pub bytes: u64,
    pub digest: Hash,
}
/// The segment files a checkpoint covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Census {
    /// Segment files whose first sequence is at most `S`.
    pub segments: u64,
    /// First sequence of the segment file that holds `S`.
    pub holding: u64,
    /// Total bytes of those files.
    pub bytes: u64,
}
#[derive(Clone, Debug)]
pub struct Checkpoint {
    pub census: Census,
    pub runs: Vec<RunRef>,
    pub state: Replay,
}
/// The stored skeleton of the active operation's tool calls: frame locators
/// only. The display projection is rebuilt from the frames.
pub type ToolFrames = (Locator, Option<Locator>, Option<Locator>);

fn locator(w: &mut Writer, value: &Locator) {
    w.bytes(&value.encode());
}
fn read_locator(r: &mut Reader<'_>) -> Result<Locator> {
    Ok(Locator::decode(r.take(24)?))
}
fn option_locator(w: &mut Writer, value: Option<&Locator>) {
    w.byte(u8::from(value.is_some()));
    if let Some(value) = value {
        locator(w, value);
    }
}
fn read_option_locator(r: &mut Reader<'_>) -> Result<Option<Locator>> {
    Ok(if r.flag()? {
        Some(read_locator(r)?)
    } else {
        None
    })
}
fn option_u64(w: &mut Writer, value: Option<u64>) {
    w.byte(u8::from(value.is_some()));
    if let Some(value) = value {
        w.u64(value);
    }
}
fn read_option_u64(r: &mut Reader<'_>) -> Result<Option<u64>> {
    Ok(if r.flag()? { Some(r.u64()?) } else { None })
}
fn terminal_kind(r: &mut Reader<'_>) -> Result<TerminalKind> {
    TerminalKind::from_code(r.byte()?).ok_or(ReplayError::Corrupt)
}
fn require(ok: bool) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(ReplayError::Corrupt)
    }
}

/// Encodes the checkpoint of `state`. The state's delta must be empty: a
/// roll writes the delta to an index run first.
pub fn encode(state: &Replay, census: &Census, runs: &[RunRef]) -> Result<Vec<u8>> {
    require(state.delta_is_empty() && runs.len() <= MAX_RUNS)?;
    let mut w = Writer(Vec::new());
    w.bytes(MAGIC);
    w.u16(1);
    w.bytes(&state.installation_id);
    // Head.
    w.u64(state.sequence);
    w.bytes(&state.last_digest);
    locator(&mut w, &state.last_frame);
    w.u64(state.revision);
    w.u64(state.owner_generation);
    // Census.
    w.u64(census.segments);
    w.u64(census.holding);
    w.u64(census.bytes);
    // Run list.
    w.u32(runs.len() as u32);
    for run in runs {
        w.u64(run.first);
        w.u64(run.last);
        w.byte(run.level);
        w.u64(run.entries);
        w.u64(run.bytes);
        w.bytes(&run.digest);
    }
    // Initialization and binding facts.
    let init = &state.initialization;
    w.bytes(&init.request);
    w.bytes(&init.digest);
    w.byte(init.mode);
    w.u64(init.configuration_revision);
    w.bytes(&init.configuration_digest);
    w.bytes(&init.graph);
    w.byte(u8::from(state.binding.is_some()));
    if let Some(binding) = &state.binding {
        w.bytes(&binding.request);
        w.u64(binding.generation);
        w.bytes(&binding.graph);
        w.bytes(&binding.configuration_digest);
    }
    // Recent and pinned projects.
    w.u32(state.projects.len() as u32);
    for (id, p) in &state.projects {
        w.bytes(id);
        let r = &p.registration;
        w.bytes(&r.request);
        w.bytes(&r.digest);
        w.string(&r.location, MAX_LOCATION)?;
        w.u64(r.device);
        w.u64(r.inode);
        w.u64(r.registry_revision);
        w.byte(r.visibility);
        locator(&mut w, &p.registered);
        w.byte(u8::from(p.name.is_some()));
        if let Some(name) = &p.name {
            w.string(&name.name, MAX_NAME)?;
            w.u64(name.revision);
        }
        w.byte(u8::from(p.latest.is_some()));
        if let Some(latest) = &p.latest {
            w.bytes(&latest.conversation);
            w.bytes(&latest.operation);
            w.u64(latest.generation);
            w.u64(latest.accepted);
            option_u64(&mut w, latest.terminal);
        }
        w.u64(p.activity);
    }
    // Recent and pinned conversations.
    w.u32(state.conversations.len() as u32);
    for (id, c) in &state.conversations {
        w.bytes(id);
        w.bytes(&c.project);
        w.u64(c.generation);
        locator(&mut w, &c.created);
        w.byte(u8::from(c.latest.is_some()));
        if let Some(latest) = &c.latest {
            w.bytes(&latest.operation);
            w.byte(latest.terminal.map_or(0, |k| k as u8));
            locator(&mut w, &latest.accepted);
        }
        w.u64(c.completed);
        w.u64(c.activity);
    }
    // The active operation: facts and frame locators only.
    match state.active() {
        None => w.byte(0),
        Some((id, op)) => {
            w.byte(1);
            w.bytes(&id);
            w.bytes(&op.conversation);
            w.u64(op.generation);
            w.bytes(&op.task);
            w.u64(op.owner_generation);
            w.bytes(&op.input_digest);
            locator(&mut w, &op.accepted_frame);
            w.optional(op.helper);
            w.byte(op.cancel.map_or(0xff, |c| c as u8));
            w.u32(op.tools.len() as u32);
            for tool in &op.tools {
                locator(&mut w, &tool.intent_frame);
                option_locator(
                    &mut w,
                    tool.exec_cmd
                        .as_deref()
                        .and_then(|e| e.settled.as_ref())
                        .map(|s| &s.frame),
                );
                option_locator(&mut w, tool.result_frame.as_ref());
            }
            // The response, then the thinking text (thinking-level.md).
            for text in [&op.response, &op.thinking] {
                w.u64(text.len);
                w.u32(text.pieces.len() as u32);
                for piece in &text.pieces {
                    w.u64(piece.start);
                    w.u64(piece.kept);
                    locator(&mut w, &piece.frame);
                }
            }
        }
    }
    // The queue.
    w.u32(state.inputs.len() as u32);
    for (id, q) in &state.inputs {
        w.bytes(id);
        require(*id == q.request)?;
        w.bytes(&q.dispatch_request);
        w.bytes(&q.project);
        w.bytes(&q.conversation);
        w.bytes(&q.target_operation);
        w.u64(q.target_generation);
        w.byte(q.kind as u8);
        locator(&mut w, &q.frame);
        w.u64(q.sequence);
        w.u64(q.owner_generation);
        w.byte(u8::from(q.held));
        w.byte(u8::from(q.dropped));
        w.byte(u8::from(q.resumed));
        w.u64(q.order_position);
        option_locator(&mut w, q.promoted.as_ref());
        w.byte(u8::from(q.dispatched.is_some()));
        if let Some((op, generation)) = q.dispatched {
            w.bytes(&op);
            w.u64(generation);
        }
        w.byte(q.dispatched_terminal.map_or(0, |k| k as u8));
        w.byte(u8::from(q.target_terminal.is_some()));
        if let Some((kind, cause)) = q.target_terminal {
            w.byte(kind as u8);
            w.byte(cause as u8);
        }
        option_u64(&mut w, q.settled);
    }
    w.u64(state.order_revision);
    w.optional(state.last_dispatched_lane);
    let digest: Hash = Sha256::digest(&w.0).into();
    w.bytes(&digest);
    w.bytes(COMMIT);
    require(w.0.len() <= MAX_CHECKPOINT)?;
    Ok(w.0)
}

/// Decodes and checks a checkpoint's bytes. `frames` rebuilds the active
/// operation's tool calls from their frames. Unknown or trailing content is
/// `Corrupt`; derived values are recomputed.
pub fn decode(bytes: &[u8], index: &mut dyn Index) -> Result<Checkpoint> {
    require(bytes.len() <= MAX_CHECKPOINT && bytes.len() > 40)?;
    let body = bytes.len() - 40;
    require(
        &bytes[bytes.len() - 8..] == COMMIT
            && bytes[body..body + 32] == Sha256::digest(&bytes[..body])[..],
    )?;
    let mut r = Reader {
        bytes: &bytes[..body],
        offset: 0,
    };
    require(r.take(8)? == MAGIC)?;
    if r.u16()? != 1 {
        return Err(ReplayError::UnsupportedFormat);
    }
    let installation_id: Id = r.array()?;
    let sequence = r.u64()?;
    let last_digest: Hash = r.array()?;
    let last_frame = read_locator(&mut r)?;
    let revision = r.u64()?;
    let owner_generation = r.u64()?;
    require(sequence > 0 && last_frame.sequence == sequence && revision == sequence)?;
    let census = Census {
        segments: r.u64()?,
        holding: r.u64()?,
        bytes: r.u64()?,
    };
    let count = r.u32()? as usize;
    require(count <= MAX_RUNS)?;
    let mut runs = Vec::with_capacity(count);
    for _ in 0..count {
        let run = RunRef {
            first: r.u64()?,
            last: r.u64()?,
            level: r.byte()?,
            entries: r.u64()?,
            bytes: r.u64()?,
            digest: r.array()?,
        };
        require(
            run.first >= 1
                && run.last >= run.first
                && run.last <= sequence
                && runs
                    .last()
                    .is_none_or(|prior: &RunRef| prior.last < run.first),
        )?;
        runs.push(run);
    }
    let initialization = PendingInit {
        request: r.array()?,
        digest: r.array()?,
        mode: r.byte()?,
        configuration_revision: r.u64()?,
        configuration_digest: r.array()?,
        graph: r.array()?,
    };
    let mut state = Replay::empty(installation_id, initialization);
    state.sequence = sequence;
    state.last_digest = last_digest;
    state.last_frame = last_frame;
    state.revision = revision;
    state.owner_generation = owner_generation;
    state.base = sequence;
    if r.flag()? {
        state.binding = Some(ActiveBinding {
            request: r.array()?,
            generation: r.u64()?,
            graph: r.array()?,
            configuration_digest: r.array()?,
        });
    }
    let count = r.u32()? as usize;
    require(count <= RECENT_PROJECTS + MAX_INPUTS + 1)?;
    for _ in 0..count {
        let id: Id = r.array()?;
        let registration = ProjectRegistered {
            request: r.array()?,
            digest: r.array()?,
            project: id,
            location: r.bounded_string(MAX_LOCATION)?,
            device: r.u64()?,
            inode: r.u64()?,
            registry_revision: r.u64()?,
            visibility: r.byte()?,
        };
        let registered = read_locator(&mut r)?;
        let name = if r.flag()? {
            Some(ProjectName {
                name: r.bounded_string(MAX_NAME)?,
                revision: r.u64()?,
            })
        } else {
            None
        };
        let latest = if r.flag()? {
            Some(ProjectLatest {
                conversation: r.array()?,
                operation: r.array()?,
                generation: r.u64()?,
                accepted: r.u64()?,
                terminal: read_option_u64(&mut r)?,
            })
        } else {
            None
        };
        let activity = r.u64()?;
        require(state.projects.last_key_value().is_none_or(|(k, _)| *k < id))?;
        state.projects.insert(
            id,
            ProjectEntry {
                registration,
                registered,
                name,
                latest,
                activity,
            },
        );
    }
    let count = r.u32()? as usize;
    require(count <= RECENT_CONVERSATIONS + MAX_INPUTS + 1)?;
    for _ in 0..count {
        let id: Id = r.array()?;
        let project = r.array()?;
        let generation = r.u64()?;
        let created = read_locator(&mut r)?;
        let latest = if r.flag()? {
            let operation = r.array()?;
            let terminal = match r.byte()? {
                0 => None,
                code => Some(TerminalKind::from_code(code).ok_or(ReplayError::Corrupt)?),
            };
            Some(LatestTurn {
                operation,
                terminal,
                accepted: read_locator(&mut r)?,
            })
        } else {
            None
        };
        let completed = r.u64()?;
        let activity = r.u64()?;
        require(
            state
                .conversations
                .last_key_value()
                .is_none_or(|(k, _)| *k < id),
        )?;
        state.conversations.insert(
            id,
            Conversation {
                project,
                generation,
                created,
                latest,
                completed,
                activity,
            },
        );
    }
    if r.flag()? {
        let id: Id = r.array()?;
        let conversation: Id = r.array()?;
        let generation = r.u64()?;
        let task = r.array()?;
        let op_owner = r.u64()?;
        let input_digest = r.array()?;
        let accepted_frame = read_locator(&mut r)?;
        let helper = r.optional()?;
        let cancel = match r.byte()? {
            0xff => None,
            code => Some(Cause::from_code(code).ok_or(ReplayError::Corrupt)?),
        };
        let count = r.u32()? as usize;
        require(count <= MAX_TOOL_CALL_ORDINAL as usize)?;
        let mut frames: Vec<ToolFrames> = Vec::with_capacity(count);
        for _ in 0..count {
            frames.push((
                read_locator(&mut r)?,
                read_option_locator(&mut r)?,
                read_option_locator(&mut r)?,
            ));
        }
        let mut texts = [Response::default(), Response::default()];
        for text in &mut texts {
            text.len = r.u64()?;
            let count = r.u32()? as usize;
            require(count <= MAX_RESPONSE_PIECES)?;
            text.pieces.reserve_exact(count);
            for _ in 0..count {
                text.pieces.push(ResponsePiece {
                    start: r.u64()?,
                    kept: r.u64()?,
                    frame: read_locator(&mut r)?,
                });
            }
        }
        let [response, thinking] = texts;
        let project = state
            .conversations
            .get(&conversation)
            .ok_or(ReplayError::Corrupt)?
            .project;
        let binding = [
            installation_id,
            state.initialization.graph,
            state.initialization.request,
        ];
        let mut tools = Vec::with_capacity(frames.len());
        for (intent, settled, result) in frames {
            let read =
                |index: &mut dyn Index, at: Locator| Ok::<_, ReplayError>((at, index.frame(&at)?));
            let intent = read(index, intent)?;
            let settled = settled.map(|at| read(index, at)).transpose()?;
            let result = result.map(|at| read(index, at)).transpose()?;
            let tool =
                tool_from_frames(installation_id, binding, project, intent, settled, result)?;
            tools.push(tool);
        }
        state.operations.insert(
            id,
            Operation {
                conversation,
                generation,
                task,
                owner_generation: op_owner,
                input_digest,
                accepted_frame,
                helper,
                cancel,
                terminal: None,
                tools,
                response,
                thinking,
            },
        );
        state.active_operation = Some(id);
    }
    let count = r.u32()? as usize;
    require(count <= MAX_INPUTS + RECENT_SETTLED_INPUTS + MAX_INPUTS)?;
    for _ in 0..count {
        let id: Id = r.array()?;
        let input = QueuedInput {
            request: id,
            dispatch_request: r.array()?,
            project: r.array()?,
            conversation: r.array()?,
            target_operation: r.array()?,
            target_generation: r.u64()?,
            kind: match r.byte()? {
                1 => InputKind::Queue,
                2 => InputKind::Steer,
                _ => return Err(ReplayError::Corrupt),
            },
            frame: read_locator(&mut r)?,
            sequence: r.u64()?,
            owner_generation: r.u64()?,
            held: r.flag()?,
            dropped: r.flag()?,
            resumed: r.flag()?,
            order_position: r.u64()?,
            promoted: read_option_locator(&mut r)?,
            dispatched: if r.flag()? {
                Some((r.array()?, r.u64()?))
            } else {
                None
            },
            dispatched_terminal: match r.byte()? {
                0 => None,
                _ => {
                    r.offset -= 1;
                    Some(terminal_kind(&mut r)?)
                }
            },
            target_terminal: if r.flag()? {
                Some((terminal_kind(&mut r)?, r.cause()?))
            } else {
                None
            },
            settled: read_option_u64(&mut r)?,
        };
        require(state.inputs.last_key_value().is_none_or(|(k, _)| *k < id))?;
        state.inputs.insert(id, input);
    }
    state.order_revision = r.u64()?;
    state.last_dispatched_lane = r.optional()?;
    r.finish()?;
    state.recompute()?;
    Ok(Checkpoint {
        census,
        runs,
        state,
    })
}
