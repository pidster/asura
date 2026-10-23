//! The execution path guard in the conversation owner
//! (execution-path-guard.md, "Warning at person dispatch", "Approval" and
//! "Control messages"; packet EPG-W).
//!
//! At each person dispatch, after the secret item read and before the job
//! admission, the owner reads the input's kind 32 (a re-dispatch after a
//! restart reuses its acknowledgement), then the project's guard state,
//! then asks the path guard worker to check the project. The entry waits
//! with `path_check` meanwhile. A `!!` command with a `model_changed`
//! warning is held with `paths_changed` (owner, 2026-10-23); every other
//! command appends kind 32 `acknowledged` and is admitted. `/paths`
//! requests (`PathGuardList`, `PathGuardApprove`, `PathGuardRevoke`) run
//! the same reads and append `approved` and `revoked`.
//!
//! Everything here is bounded in-memory work on the reactor (AE-01): the
//! walks and digests run on the worker, the records on the writer worker.
//! No wait has a time bound (AE-04): a dispatch check ends by its reply,
//! a Hold or Drop of the entry (the ticket's drop cancels it), or shutdown.
use super::person_command::Dispatch;
use super::{Body, Job, Owner, reply};
use crate::path_guard::check::{self, Facts, Warnings};
use crate::path_guard::walk::FileState;
use crate::path_guard::worker::{Guard, Polled, Refused, Reply, Root, Ticket, Work};
use crate::path_guard::lists;
use crate::secret_store::Secrets;
use asura_control::path_guard::{MAX_PATH_WARNINGS, error as token, path_form_valid};
use asura_control::pb;
use asura_storage::authority::conversation::{
    ExecutionPathRecord, Id, InputStatus, PathAction, PathEntry, PathEntryKind, PathGuardView,
    PersonCommandQueued,
};
use asura_storage::authority::writer;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, VecDeque};
use std::time::Instant;

/// The hold reason of a `!!` command with a `model_changed` warning.
pub(super) const PATHS_CHANGED: &str = "paths_changed";
/// The waiting text of a dispatch check.
const PATH_CHECK_TEXT: &str = "waiting: path check";
/// The waiting text of an acknowledgement held at the journal quota.
const JOURNAL_HELD_TEXT: &str = "waiting: journal held";
/// `/paths` requests in progress at once; another is `busy`.
const CONTROL_BOUND: usize = 4;
/// Encoded warning bytes in one queue reply: with 16 entries of at most 16
/// warnings of 1,024 bytes the reply would pass one frame, so the listed
/// warnings stop here and the rest are counted in `path_overflow`.
pub(super) const QUEUE_WARNING_BYTES: usize = 24 * 1024;
/// Encoded approved-path bytes in one `PathGuardReply`.
const APPROVED_BYTES: usize = 40 * 1024;

/// The path guard step of one person dispatch.
pub(super) enum GuardStep {
    /// The input's kind 32 read waits for the journal ticket.
    Record,
    RecordReading,
    /// The project's guard state read waits for the ticket.
    View,
    ViewReading,
    /// The check waits for room in the worker's queue.
    Check(Box<PathGuardView>),
    Checking(Box<PathGuardView>, Ticket),
    /// The acknowledgement waits for the ticket, or for the journal hold
    /// to end.
    Ack(Box<ExecutionPathRecord>),
    Acking,
}

/// One `/paths` request.
enum Op {
    List,
    /// `path` is `None` for `all`.
    Approve {
        reference: Id,
        path: Option<String>,
    },
    Revoke {
        reference: Id,
        path: String,
    },
}
enum Step {
    /// A journal read waits for the ticket.
    Record,
    View,
    /// A journal read or append is in flight.
    Flight,
    /// The worker request waits for room.
    Work(Box<PathGuardView>),
    Working(Box<PathGuardView>, Ticket),
    /// Appends wait for the ticket, in order.
    Append(VecDeque<ExecutionPathRecord>),
}
struct Control {
    key: u64,
    request: pb::Envelope,
    project: Id,
    op: Op,
    /// The mutation is done (or was none): the next view and check give
    /// the reply.
    listing: bool,
    step: Step,
}

/// The path guard state of the conversation owner. Live display state
/// only; the journal (kind 32) is the source of every durable fact.
#[derive(Default)]
pub(super) struct Paths {
    guard: Option<Guard>,
    guard_failed: bool,
    /// Inputs whose acknowledgement committed (or was found): their start
    /// proceeds.
    acknowledged: std::collections::BTreeSet<Id>,
    /// The warnings shown on each live person entry.
    shown: BTreeMap<Id, Warnings>,
    /// A `!!` entry held with `paths_changed`: the newest `model_changed`
    /// sequence its warnings showed. Resume admits it when no newer one
    /// appeared.
    held_through: BTreeMap<Id, u64>,
    controls: VecDeque<Control>,
    next_key: u64,
}

impl Paths {
    /// Whether `input` passed its check (the start proceeds).
    pub(super) fn acknowledged(&self, input: &Id) -> bool {
        self.acknowledged.contains(input)
    }
    /// The warnings of a live entry.
    pub(super) fn shown(&self, input: &Id) -> Option<&Warnings> {
        self.shown.get(input)
    }
}

/// The queue entry fields 21-22 of `warnings`, within the reply's
/// remaining warning bytes: a cut list counts the rest in the overflow.
pub(super) fn entry_fields(
    warnings: Option<&Warnings>,
    budget: &mut usize,
) -> (Vec<pb::PathWarning>, Option<u32>) {
    let Some(warnings) = warnings else {
        return (Vec::new(), None);
    };
    let mut listed = Vec::new();
    let mut omitted = u64::from(warnings.overflow);
    for w in &warnings.listed {
        let cost = w.path.len() + 8;
        if cost > *budget || listed.len() == MAX_PATH_WARNINGS {
            omitted += 1;
            continue;
        }
        *budget -= cost;
        listed.push(pb::PathWarning {
            kind: Some(w.kind as u32),
            path: Some(w.path.clone()),
        });
    }
    let overflow = (omitted > 0).then(|| u32::try_from(omitted).unwrap_or(u32::MAX));
    (listed, overflow)
}

/// The reference of one record of `/paths approve all`: one kind 32 per
/// reference (AE-05), so each file's approval derives its own from the
/// request ID and the path, and a retry of the request derives the same.
fn derived_reference(request: &Id, path: &str) -> Id {
    let mut hash = Sha256::new();
    hash.update(b"asura.path-approve-all\0");
    hash.update(request);
    hash.update(path.as_bytes());
    let digest: [u8; 32] = hash.finalize().into();
    let mut id = [0; 16];
    id.copy_from_slice(&digest[..16]);
    // An all-zero reference is invalid in kind 32.
    if id == [0; 16] {
        id[15] = 1;
    }
    id
}

fn approval(
    action: PathAction,
    project: Id,
    reference: Id,
    path: &str,
    digest: [u8; 32],
) -> ExecutionPathRecord {
    ExecutionPathRecord {
        action,
        project,
        reference,
        through: 0,
        entries: vec![PathEntry {
            kind: PathEntryKind::Approval,
            path: path.to_owned(),
        }],
        overflow: 0,
        digest,
    }
}

fn error_reply(request: pb::Envelope, error: &str) -> pb::Envelope {
    reply(
        request,
        Body::PathGuardReply(pb::PathGuardReply {
            pending: Vec::new(),
            overflow: None,
            approved: Vec::new(),
            error: Some(error.to_owned()),
        }),
    )
}

fn read_error(cause: writer::Error) -> &'static str {
    match cause {
        writer::Error::Invalid => token::NOT_FOUND,
        writer::Error::Limit => token::LIMIT,
        _ => token::UNAVAILABLE,
    }
}

impl Owner {
    // ----- The worker -----

    /// Starts the worker once the reactor's wake is known. A failed start
    /// leaves every check waiting with `path_check` and every request
    /// `unavailable`; the next poll does not retry (no loop).
    fn path_guard(&mut self) -> Option<&Guard> {
        if self.paths.guard.is_none() && !self.paths.guard_failed && self.wake.is_some() {
            #[cfg(feature = "test-support")]
            let gate = self
                .runtime
                .scratch_home_for_test()
                .map(|home| home.join("path-guard-hold"));
            #[cfg(not(feature = "test-support"))]
            let gate = None;
            match Guard::start(self.wake.clone(), gate) {
                Ok(guard) => self.paths.guard = Some(guard),
                Err(_) => {
                    self.paths.guard_failed = true;
                    tracing::warn!("path_guard_unavailable");
                }
            }
        }
        self.paths.guard.as_ref()
    }

    fn guard_root(&self, project: Id) -> Option<Root> {
        let (path, device, inode, boundary) = self.project_identity(project)?;
        Some(Root {
            path,
            device,
            inode,
            boundary,
        })
    }

    /// Shutdown: the worker's queue closes (AE-06).
    pub(super) fn close_path_guard(&mut self) {
        if let Some(guard) = &mut self.paths.guard {
            guard.close();
        }
        for control in std::mem::take(&mut self.paths.controls) {
            self.out
                .push_back(error_reply(control.request, token::UNAVAILABLE));
        }
    }

    // ----- Dispatch -----

    /// One step of a dispatch's path check (EPG-Q1).
    pub(super) fn advance_guard(
        &mut self,
        record: Box<PersonCommandQueued>,
        values: Option<Box<Secrets>>,
        step: GuardStep,
    ) -> Dispatch {
        let step = match step {
            GuardStep::Check(view) => {
                let root = self.guard_root(record.project);
                let approved = view.approvals.iter().map(|a| a.path.clone()).collect();
                match (root, self.path_guard()) {
                    (Some(root), Some(guard)) if guard.has_room() => match guard
                        .submit(root, Work::Check { approved })
                    {
                        Ok(ticket) => GuardStep::Checking(view, ticket),
                        // A full queue: the entry waits with `path_check`
                        // until the worker accepts; never refused.
                        Err(Refused::Busy | Refused::Stopped) => GuardStep::Check(view),
                    },
                    // Not resident, or no worker: the check waits.
                    _ => GuardStep::Check(view),
                }
            }
            GuardStep::Checking(view, ticket) => match ticket.poll() {
                Polled::Pending => GuardStep::Checking(view, ticket),
                Polled::Lost | Polled::Ready(Reply::Cancelled) => GuardStep::Check(view),
                Polled::Ready(Reply::Checked(facts)) => {
                    return self.path_checked(record, values, &view, &facts);
                }
                // The project cannot be read: nothing can be verified; the
                // approvals show as changed and the walk as overflow.
                Polled::Ready(Reply::Unavailable | Reply::Digest(..)) => {
                    let facts = Facts {
                        overflow: true,
                        ..Facts::default()
                    };
                    return self.path_checked(record, values, &view, &facts);
                }
            },
            other => other,
        };
        Dispatch::Guard(record, values, step)
    }

    /// The check's warnings: a `!!` command with a new `model_changed`
    /// warning is held; any other command is acknowledged.
    fn path_checked(
        &mut self,
        record: Box<PersonCommandQueued>,
        values: Option<Box<Secrets>>,
        view: &PathGuardView,
        facts: &Facts,
    ) -> Dispatch {
        let input = record.input;
        let warnings = Warnings::from_all(check::all(view, facts));
        let through = check::model_through(view);
        let shown = self.paths.held_through.get(&input).copied();
        if record.outside_sandbox
            && warnings.model_changed()
            && shown.is_none_or(|shown| through > shown)
        {
            self.paths.held_through.insert(input, through);
            self.paths.shown.insert(input, warnings);
            drop(values);
            return self.command_hold(input, PATHS_CHANGED);
        }
        let ack = check::acknowledgement(record.project, input, view, &warnings);
        self.paths.shown.insert(input, warnings);
        Dispatch::Guard(record, values, GuardStep::Ack(Box::new(ack)))
    }

    /// Takes the journal ticket for the next path guard read or append:
    /// dispatch steps first, then `/paths` requests. Returns whether it
    /// took the ticket.
    pub(super) fn drive_path_journal(&mut self) -> bool {
        if self.pending.is_some() || self.unavailable.is_some() || self.closing {
            return false;
        }
        let held = self.journal_held();
        let next = self.persons.dispatches.iter().find_map(|(input, d)| match d {
            Dispatch::Guard(_, _, GuardStep::Record | GuardStep::View) => Some(*input),
            Dispatch::Guard(_, _, GuardStep::Ack(_)) if !held => Some(*input),
            _ => None,
        });
        if let Some(input) = next {
            let Some(Dispatch::Guard(record, values, step)) = self.persons.dispatches.remove(&input)
            else {
                return false;
            };
            let (command, step) = match step {
                GuardStep::Record => (
                    Some(writer::Command::PathRecord { reference: input }),
                    GuardStep::RecordReading,
                ),
                GuardStep::View => (
                    Some(writer::Command::PathGuard {
                        project: record.project,
                    }),
                    GuardStep::ViewReading,
                ),
                GuardStep::Ack(ack) => {
                    let revision = self.replay.as_ref().map_or(0, |r| r.revision);
                    (
                        Some(writer::Command::Append {
                            expected_revision: revision,
                            record: asura_storage::authority::conversation::Record::ExecutionPath(
                                ack,
                            ),
                            admission: None,
                        }),
                        GuardStep::Acking,
                    )
                }
                other => (None, other),
            };
            let Some(command) = command else {
                self.persons
                    .dispatches
                    .insert(input, Dispatch::Guard(record, values, step));
                return false;
            };
            if self.submit(command, Job::PathDispatch(input)).is_err() {
                // Read again from the start on the next pass.
                return false;
            }
            self.persons
                .dispatches
                .insert(input, Dispatch::Guard(record, values, step));
            return true;
        }
        self.drive_path_controls(held)
    }

    /// A dispatch read or acknowledgement completed.
    pub(super) fn path_dispatch_done(
        &mut self,
        input: Id,
        result: writer::Result<writer::Reply>,
        now: Instant,
    ) {
        let Some(Dispatch::Guard(record, values, step)) = self.persons.dispatches.remove(&input)
        else {
            // Held or dropped meanwhile.
            return;
        };
        let next = match (step, result) {
            (GuardStep::RecordReading, Ok(value)) => match value.path_record {
                Some((_, ack)) if ack.action == PathAction::Acknowledged => {
                    // A re-dispatch (after a restart, or a later Resume):
                    // the acknowledgement stands.
                    self.paths
                        .shown
                        .insert(input, Warnings::from_record(&ack));
                    self.paths.acknowledged.insert(input);
                    Dispatch::Admit(record, values)
                }
                _ => Dispatch::Guard(record, values, GuardStep::View),
            },
            (GuardStep::ViewReading, Ok(value)) => match value.path_guard {
                Some(view) => Dispatch::Guard(record, values, GuardStep::Check(view)),
                None => Dispatch::Guard(record, values, GuardStep::View),
            },
            (GuardStep::Acking, Ok(_)) => {
                self.paths.held_through.remove(&input);
                self.paths.acknowledged.insert(input);
                Dispatch::Admit(record, values)
            }
            // Held at the journal quota: the command waits with it, and
            // checks again once the hold ends (its acknowledgement waits
            // for the ticket while the journal holds normal records).
            (
                GuardStep::Acking,
                Err(writer::Error::JournalQuota | writer::Error::JournalDiskSpace),
            ) => Dispatch::Guard(record, values, GuardStep::View),
            // Another record under the input's reference: read it.
            (GuardStep::Acking, Err(writer::Error::Conflict)) => {
                Dispatch::Guard(record, values, GuardStep::Record)
            }
            (_, Err(cause)) => {
                tracing::info!(class = super::storage_error(cause), "path_check_read_failed");
                // The dispatch starts again on the next pass.
                return;
            }
            (step, Ok(_)) => Dispatch::Guard(record, values, step),
        };
        self.persons.dispatches.insert(input, next);
        self.advance_dispatch(input, now);
    }

    /// The waiting reason of a dispatch in its path check.
    pub(super) fn guard_waiting(&self, step: &GuardStep) -> (i32, String) {
        match step {
            GuardStep::Ack(_) if self.journal_held() => (
                pb::QueueWaitReason::JournalHeld as i32,
                JOURNAL_HELD_TEXT.to_owned(),
            ),
            _ => (
                pb::QueueWaitReason::PathCheck as i32,
                PATH_CHECK_TEXT.to_owned(),
            ),
        }
    }

    /// Drops the live state of inputs that are no longer live; a held
    /// entry keeps its warnings, a dropped one loses its hold mark.
    pub(super) fn poll_paths(&mut self) {
        let Some(state) = self.replay.as_deref() else {
            return;
        };
        let live = |input: &Id| {
            matches!(
                state.input_status(*input),
                Some(InputStatus::Queued | InputStatus::Held | InputStatus::Running)
            )
        };
        self.paths.shown.retain(|input, _| live(input));
        self.paths.acknowledged.retain(|input| live(input));
        self.paths.held_through.retain(|input, _| {
            matches!(
                state.input_status(*input),
                Some(InputStatus::Queued | InputStatus::Held)
            )
        });
        self.poll_path_controls();
    }

    // ----- `/paths` requests -----

    /// One `PathGuardList`, `PathGuardApprove` or `PathGuardRevoke`.
    pub(super) fn path_request(&mut self, request: pb::Envelope) -> Option<pb::Envelope> {
        let (project, op) = match request.body.as_ref() {
            Some(Body::PathGuardList(v)) => (super::id(&v.project_id), Op::List),
            Some(Body::PathGuardApprove(v)) => {
                let path = match (&v.path, v.all) {
                    (_, Some(true)) => None,
                    (Some(path), _) => {
                        if !path_form_valid(path) {
                            return Some(error_reply(request, token::PATH_INVALID));
                        }
                        if !lists::implicit(path) {
                            return Some(error_reply(request, token::NOT_IMPLICIT));
                        }
                        Some(path.clone())
                    }
                    (None, _) => return Some(error_reply(request, token::PATH_INVALID)),
                };
                (
                    super::id(&v.project_id),
                    Op::Approve {
                        reference: super::id(&v.request_id),
                        path,
                    },
                )
            }
            Some(Body::PathGuardRevoke(v)) => {
                let path = v.path.clone().unwrap_or_default();
                if !path_form_valid(&path) {
                    return Some(error_reply(request, token::PATH_INVALID));
                }
                (
                    super::id(&v.project_id),
                    Op::Revoke {
                        reference: super::id(&v.request_id),
                        path,
                    },
                )
            }
            _ => return Some(error_reply(request, token::UNAVAILABLE)),
        };
        if self.paths.controls.len() >= CONTROL_BOUND {
            return Some(error_reply(request, token::BUSY));
        }
        let step = match op {
            Op::List | Op::Approve { path: None, .. } => Step::View,
            Op::Approve { .. } | Op::Revoke { .. } => Step::Record,
        };
        self.paths.next_key += 1;
        self.paths.controls.push_back(Control {
            key: self.paths.next_key,
            request,
            project,
            listing: matches!(op, Op::List),
            op,
            step,
        });
        None
    }

    fn control_reply(&mut self, key: u64, body: pb::Envelope) {
        self.paths.controls.retain(|c| c.key != key);
        self.out.push_back(body);
    }

    /// Takes the ticket for the oldest `/paths` request that needs it.
    fn drive_path_controls(&mut self, held: bool) -> bool {
        let Some(index) = self.paths.controls.iter().position(|c| match &c.step {
            Step::Record | Step::View => true,
            Step::Append(_) => !held,
            _ => false,
        }) else {
            return false;
        };
        let revision = self.replay.as_ref().map_or(0, |r| r.revision);
        let control = &mut self.paths.controls[index];
        let key = control.key;
        let command = match std::mem::replace(&mut control.step, Step::Flight) {
            Step::Record => {
                let reference = match &control.op {
                    Op::Approve { reference, .. } | Op::Revoke { reference, .. } => *reference,
                    Op::List => [0; 16],
                };
                writer::Command::PathRecord { reference }
            }
            Step::View => writer::Command::PathGuard {
                project: control.project,
            },
            Step::Append(mut records) => {
                let record = records.pop_front().expect("an append");
                control.step = Step::Append(records);
                writer::Command::Append {
                    expected_revision: revision,
                    record: asura_storage::authority::conversation::Record::ExecutionPath(
                        Box::new(record),
                    ),
                    admission: None,
                }
            }
            _ => unreachable!("selected above"),
        };
        if let Err(e) = self.submit(command, Job::PathControl(key)) {
            let request = self.paths.controls[index].request.clone();
            self.control_reply(key, error_reply(request, read_error(e)));
            return false;
        }
        true
    }

    /// A `/paths` read or append completed.
    pub(super) fn path_control_done(&mut self, key: u64, result: writer::Result<writer::Reply>) {
        let Some(index) = self.paths.controls.iter().position(|c| c.key == key) else {
            return;
        };
        let control = &mut self.paths.controls[index];
        let appending = matches!(control.step, Step::Append(_));
        match result {
            Err(cause) if appending => {
                let Step::Append(rest) = std::mem::replace(&mut control.step, Step::Flight) else {
                    unreachable!("appending");
                };
                match cause {
                    // `all`: a file approved under this request with other
                    // content; its warning stays.
                    writer::Error::Conflict => {
                        control.step = if rest.is_empty() {
                            control.listing = true;
                            Step::View
                        } else {
                            Step::Append(rest)
                        };
                    }
                    cause => {
                        let request = control.request.clone();
                        let error = match cause {
                            writer::Error::Limit => token::LIMIT,
                            _ => token::UNAVAILABLE,
                        };
                        self.control_reply(key, error_reply(request, error));
                    }
                }
            }
            Err(cause) => {
                let request = control.request.clone();
                self.control_reply(key, error_reply(request, read_error(cause)));
            }
            Ok(value) => {
                if appending {
                    let Step::Append(rest) = &control.step else {
                        unreachable!("appending");
                    };
                    if rest.is_empty() {
                        control.listing = true;
                        control.step = Step::View;
                    }
                    return;
                }
                if let Some(view) = value.path_guard {
                    self.path_control_view(index, view);
                } else {
                    // The request's own record: an exact retry lists.
                    let found = value.path_record.is_some();
                    let control = &mut self.paths.controls[index];
                    if found {
                        control.listing = true;
                    }
                    control.step = Step::View;
                }
            }
        }
    }

    /// The project's guard state for a request.
    fn path_control_view(&mut self, index: usize, view: Box<PathGuardView>) {
        let control = &mut self.paths.controls[index];
        let key = control.key;
        if control.listing {
            control.step = Step::Work(view);
            return;
        }
        match &control.op {
            Op::Revoke { reference, path } => {
                if !view.approvals.iter().any(|a| &a.path == path) {
                    let request = control.request.clone();
                    return self.control_reply(key, error_reply(request, token::NOT_FOUND));
                }
                if !view.has_room(PathAction::Revoked, path) {
                    let request = control.request.clone();
                    return self.control_reply(key, error_reply(request, token::LIMIT));
                }
                let record = approval(
                    PathAction::Revoked,
                    control.project,
                    *reference,
                    path,
                    [0; 32],
                );
                control.step = Step::Append(VecDeque::from([record]));
            }
            Op::Approve { .. } | Op::List => control.step = Step::Work(view),
        }
    }

    /// The worker step of each `/paths` request.
    fn poll_path_controls(&mut self) {
        for index in 0..self.paths.controls.len() {
            let Some(control) = self.paths.controls.get(index) else {
                break;
            };
            let project = control.project;
            let key = control.key;
            match &control.step {
                Step::Work(view) => {
                    let work = match (&control.op, control.listing) {
                        (Op::Approve { path: Some(path), .. }, false) => Work::Digest {
                            path: path.clone(),
                        },
                        _ => Work::Check {
                            approved: view.approvals.iter().map(|a| a.path.clone()).collect(),
                        },
                    };
                    let Some(root) = self.guard_root(project) else {
                        let request = control.request.clone();
                        self.control_reply(key, error_reply(request, token::UNAVAILABLE));
                        return;
                    };
                    let submitted = match self.path_guard() {
                        Some(guard) => guard.submit(root, work),
                        None => Err(Refused::Stopped),
                    };
                    let control = &mut self.paths.controls[index];
                    match submitted {
                        Ok(ticket) => {
                            let Step::Work(view) = std::mem::replace(&mut control.step, Step::Flight)
                            else {
                                unreachable!("work");
                            };
                            control.step = Step::Working(view, ticket);
                        }
                        Err(Refused::Busy) => {
                            let request = control.request.clone();
                            self.control_reply(key, error_reply(request, token::BUSY));
                            return;
                        }
                        Err(Refused::Stopped) => {
                            let request = control.request.clone();
                            self.control_reply(key, error_reply(request, token::UNAVAILABLE));
                            return;
                        }
                    }
                }
                Step::Working(_, ticket) => {
                    let polled = ticket.poll();
                    if polled == Polled::Pending {
                        continue;
                    }
                    let control = &mut self.paths.controls[index];
                    let Step::Working(view, _) = std::mem::replace(&mut control.step, Step::Flight)
                    else {
                        unreachable!("working");
                    };
                    self.path_control_worked(index, view, polled);
                    return;
                }
                _ => {}
            }
        }
    }

    /// A `/paths` request's worker reply.
    fn path_control_worked(&mut self, index: usize, view: Box<PathGuardView>, polled: Polled) {
        let control = &mut self.paths.controls[index];
        let key = control.key;
        let request = control.request.clone();
        let reply_now = |owner: &mut Self, error: &str| {
            owner.control_reply(key, error_reply(request.clone(), error));
        };
        let worked = match polled {
            Polled::Ready(reply) => reply,
            Polled::Lost | Polled::Pending => return reply_now(self, token::UNAVAILABLE),
        };
        if control.listing {
            return match worked {
                Reply::Checked(facts) => {
                    let body = list_reply(&view, &facts);
                    self.control_reply(key, reply(request, Body::PathGuardReply(body)));
                }
                _ => reply_now(self, token::UNAVAILABLE),
            };
        }
        let project = control.project;
        match (&control.op, worked) {
            (Op::Approve { reference, path: Some(_) }, Reply::Digest(state, shown)) => {
                let reference = *reference;
                let record = match (state, shown) {
                    (FileState::Digest(digest), Some(shown)) => {
                        if !view.has_room(PathAction::Approved, &shown) {
                            return reply_now(self, token::LIMIT);
                        }
                        approval(PathAction::Approved, project, reference, &shown, digest)
                    }
                    (FileState::TooLarge, _) => return reply_now(self, token::TOO_LARGE),
                    (FileState::Missing, _) => return reply_now(self, token::NOT_FOUND),
                    _ => return reply_now(self, token::UNAVAILABLE),
                };
                self.paths.controls[index].step = Step::Append(VecDeque::from([record]));
            }
            (Op::Approve { reference, path: None }, Reply::Checked(facts)) => {
                let reference = *reference;
                let mut records = VecDeque::new();
                let mut room = view.as_ref().clone();
                for (path, digest) in check::approvable(&view, &facts) {
                    if !room.has_room(PathAction::Approved, &path) {
                        if records.is_empty() {
                            return reply_now(self, token::LIMIT);
                        }
                        break;
                    }
                    room.approval_records += 1;
                    if !room.approvals.iter().any(|a| a.path == path) {
                        room.approvals.push(asura_storage::authority::conversation::PathApproval {
                            path: path.clone(),
                            digest,
                            frame: Default::default(),
                        });
                    }
                    records.push_back(approval(
                        PathAction::Approved,
                        project,
                        derived_reference(&reference, &path),
                        &path,
                        digest,
                    ));
                }
                let control = &mut self.paths.controls[index];
                if records.is_empty() {
                    control.listing = true;
                    control.step = Step::Work(view);
                } else {
                    control.step = Step::Append(records);
                }
            }
            _ => reply_now(self, token::UNAVAILABLE),
        }
    }
}

/// The reply to a list: the warnings the next person command would show,
/// and the approved paths in byte order within the reply's bytes.
fn list_reply(view: &PathGuardView, facts: &Facts) -> pb::PathGuardReply {
    let warnings = Warnings::from_all(check::all(view, facts));
    let mut budget = usize::MAX;
    let (pending, overflow) = entry_fields(Some(&warnings), &mut budget);
    let mut approved = Vec::new();
    let mut bytes = 0;
    for a in &view.approvals {
        bytes += a.path.len() + 4;
        if bytes > APPROVED_BYTES {
            break;
        }
        approved.push(a.path.clone());
    }
    pb::PathGuardReply {
        pending,
        overflow,
        approved,
        error: None,
    }
}
