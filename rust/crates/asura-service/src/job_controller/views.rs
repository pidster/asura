//! Job views (tool-job-controller.md, J3 kind 29 and J4 listings): the
//! state block of one job, the live jobs of a scope with their kind 26
//! sequence, the live counts and the job revision. The outlived-turn count
//! (EX12) is derived from the journal records at each terminal read, not
//! here. Every function is bounded in-memory work on the
//! reactor (AE-01): at most the 64 unsettled jobs and the 64 remembered
//! settled jobs.
//!
//! The conversation owner and the job control handler are the callers.
use super::*;
use journal::{JobBlockReason, JobListEntry, JobRunState, JobScope, JobSnapshot};

/// What the controller knows of one job, in the kind 29 vocabulary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct JobView {
    pub job: Id,
    pub origin: Origin,
    pub agent: AgentKey,
    pub project: Id,
    /// The kind 26 frame sequence; `None` until kind 26 commits.
    pub sequence: Option<u64>,
    /// Kind 26 field 4 (Unix milliseconds); 0 until kind 26 is built.
    pub intent_ms: u64,
    /// The state block. State 9 (`settled`) only after kind 28 committed
    /// (J3-E7); its facts are then read from kind 28.
    pub snapshot: JobSnapshot,
    /// The brief text of a queued job's reason with its live numbers
    /// (AQ1), for example `waiting: 2 of 2 jobs running for this agent`.
    pub text: Option<String>,
    /// The first accepted cancel trigger (kind 27 was or will be written).
    pub cancel: Option<JobReason>,
    /// The kind 28 outcome once it is built.
    pub outcome: Option<Outcome>,
    /// The `JobEntry.command` excerpt built at admission (storage's
    /// `command_excerpt`); empty for a recovered job, whose excerpt comes
    /// from a journal read.
    pub excerpt: String,
}
impl JobView {
    /// Kind 28 committed: the settled facts are in the journal.
    pub(crate) fn settled(&self) -> bool {
        self.snapshot.state == JobRunState::Settled
    }
    /// The kind 29 list entry (code 11). A settled entry stores zero
    /// bytes; its facts come from kind 28.
    pub(crate) fn list_entry(&self) -> JobListEntry {
        JobListEntry {
            job: self.job,
            state: self.snapshot.state,
            numbers: self.snapshot.numbers,
            stdout_bytes: self.snapshot.stdout_bytes,
            stderr_bytes: self.snapshot.stderr_bytes,
        }
    }
}

impl JobController {
    /// The job revision. A parked `JobList` with `after_revision` returns
    /// when it differs.
    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }
    /// The owner's retention (kind 30 committed) and execution setting
    /// changes increment the revision; the controller's own events do so
    /// itself.
    pub(crate) fn bump_revision(&mut self) {
        self.revision = self.revision.wrapping_add(1).max(1);
    }

    /// The view of a job this controller holds or remembers. `None` for a
    /// job it never held, settled before this run or more than 64
    /// settlements ago, or withdrawn: the caller reads the journal.
    pub(crate) fn view(&self, job: Id, now: Instant) -> Option<JobView> {
        if let Some(j) = self.jobs.get(&job) {
            return Some(self.job_view(j, now));
        }
        self.retired.iter().find(|r| r.job == job).map(|r| JobView {
            job: r.job,
            origin: r.origin,
            agent: r.agent,
            project: r.project,
            sequence: r.sequence,
            intent_ms: r.intent_ms,
            snapshot: settled_snapshot(),
            text: None,
            cancel: None,
            outcome: Some(r.outcome),
            excerpt: r.excerpt.clone(),
        })
    }

    /// The committed kind 28 of a job this controller holds or remembers;
    /// `None` before kind 28 commits or for a job it does not know.
    pub(crate) fn settled_facts(&self, job: Id) -> Option<&JobSettled> {
        match self.jobs.get(&job) {
            Some(j) if j.state == JobState::Recorded => j.settled.as_deref(),
            Some(_) => None,
            None => self
                .retired
                .iter()
                .find(|r| r.job == job)
                .and_then(|r| r.settled.as_deref()),
        }
    }

    /// The live jobs of `scope` (every project when `None`), newest first
    /// by kind 26 sequence. Live: kind 26 committed and kind 28 not
    /// committed, journal available. A person waiter with no kind 26 has
    /// no sequence and is not listed.
    pub(crate) fn live_views(&self, scope: Option<JobScope>, now: Instant) -> Vec<JobView> {
        let mut views: Vec<JobView> = self
            .jobs
            .values()
            .filter(|j| live(j) && in_scope(j, scope))
            .map(|j| self.job_view(j, now))
            .collect();
        views.sort_by_key(|v| std::cmp::Reverse(v.sequence));
        views
    }

    /// The number of live jobs in `scope` (every project when `None`):
    /// `JobListReply.live_in_scope` and `live_in_service`, the status-row
    /// count and the exit-confirmation count.
    pub(crate) fn live_count(&self, scope: Option<JobScope>) -> u32 {
        let n = self
            .jobs
            .values()
            .filter(|j| live(j) && in_scope(j, scope))
            .count();
        u32::try_from(n).unwrap_or(u32::MAX)
    }

    fn job_view(&self, j: &Job, now: Instant) -> JobView {
        let state = self.run_state(j);
        let snapshot = if state == JobRunState::Settled {
            settled_snapshot()
        } else {
            JobSnapshot {
                state,
                numbers: self.numbers(j, state),
                elapsed_ms: if state.before_spawn() {
                    0
                } else {
                    j.spawned_at.map_or(0, |at| {
                        u64::try_from(now.saturating_duration_since(at).as_millis())
                            .unwrap_or(u64::MAX)
                    })
                },
                stdout_bytes: j.stored[0],
                stderr_bytes: j.stored[1],
            }
        };
        let text = match state {
            JobRunState::Queued(_) => {
                let pending = self.seen.map_or(0, |s| s.1);
                Some(block_text(
                    j.block.unwrap_or_else(|| self.unevaluated_block(j)),
                    &self.counts(j.agent, pending),
                ))
            }
            _ => None,
        };
        JobView {
            job: j.id,
            origin: j.origin,
            agent: j.agent,
            project: j.project,
            sequence: j.sequence,
            intent_ms: j.intent_ms,
            snapshot,
            text,
            cancel: j.cancel.map(|t| t.reason),
            outcome: j.settled.as_ref().map(|s| s.outcome),
            excerpt: j.excerpt.clone(),
        }
    }

    /// The state block state of a job. A built but uncommitted kind 28,
    /// and a restart settlement still to commit, are `finalizing`.
    pub(super) fn run_state(&self, j: &Job) -> JobRunState {
        match j.state {
            JobState::Queued => JobRunState::Queued(
                j.block
                    .unwrap_or_else(|| self.unevaluated_block(j))
                    .journal(),
            ),
            JobState::Starting => JobRunState::Starting,
            JobState::Running => JobRunState::Running,
            JobState::CapturePaused => {
                JobRunState::CapturePaused(j.pause.unwrap_or(journal::JobPauseReason::Quota))
            }
            JobState::Stopping => JobRunState::Stopping,
            JobState::Draining => JobRunState::Draining,
            JobState::CleanupUnconfirmed => JobRunState::CleanupUnconfirmed,
            JobState::Finalizing
            | JobState::Recording
            | JobState::Interrupted
            | JobState::Withdrawn => JobRunState::Finalizing,
            JobState::Recorded => JobRunState::Settled,
        }
    }

    /// The reason numbers: running, limit and awaiting cleanup for the
    /// slot reasons; the pending-install count for `pending_installs`.
    fn numbers(&self, j: &Job, state: JobRunState) -> [u8; 3] {
        let pending = self.seen.map_or(0, |s| s.1);
        let c = self.counts(j.agent, pending);
        let byte = |n: usize| u8::try_from(n).unwrap_or(u8::MAX);
        match state {
            JobRunState::Queued(JobBlockReason::AgentSlots) => [
                byte(c.agent_live),
                byte(c.agent_limit),
                byte(c.agent_awaiting_cleanup),
            ],
            JobRunState::Queued(JobBlockReason::ServiceSlots) => [
                byte(c.service_live),
                byte(c.service_limit),
                byte(c.service_awaiting_cleanup),
            ],
            JobRunState::Queued(JobBlockReason::PendingInstalls) => {
                [byte(c.pending_installs), 0, 0]
            }
            _ => [0; 3],
        }
    }

    /// A queued job that the head evaluation has not reached since its
    /// kind 26 committed (the next poll evaluates it): the first start
    /// condition that fails on the controller's cached values, else the
    /// reason of its agent queue's head, else `agent_slots`. The owner
    /// polls before it takes a snapshot for a result, so a result reads
    /// an evaluated reason.
    fn unevaluated_block(&self, j: &Job) -> BlockReason {
        let pending = self.seen.map_or(0, |s| s.1);
        let c = self.counts(j.agent, pending);
        if self.recovering {
            BlockReason::Recovering
        } else if self.journal_down {
            BlockReason::JournalUnavailable
        } else if self.journal_held {
            BlockReason::JournalHeld
        } else if self.seen.is_some_and(|s| !s.0) {
            BlockReason::PayloadUnavailable
        } else if c.agent_live >= c.agent_limit {
            BlockReason::AgentSlots
        } else if c.service_live >= c.service_limit {
            BlockReason::ServiceSlots
        } else if c.pending_installs >= PENDING_INSTALLS {
            BlockReason::PendingInstalls
        } else {
            self.queues
                .get(&j.agent)
                .and_then(|q| q.front())
                .and_then(|head| self.jobs.get(head))
                .and_then(|head| head.block)
                .unwrap_or(BlockReason::AgentSlots)
        }
    }
}

/// Kind 26 committed, kind 28 not committed, and the journal still
/// available: the job is live for listings and counts.
fn live(j: &Job) -> bool {
    j.intent == Intent::Committed
        && !j.abandoned
        && !j.recovered
        && !matches!(j.state, JobState::Recorded | JobState::Withdrawn)
}

fn in_scope(j: &Job, scope: Option<JobScope>) -> bool {
    match scope {
        None | Some(JobScope::AllProjects) => true,
        Some(JobScope::Project(project)) => j.project == project,
        Some(JobScope::Conversation(conversation)) => j.agent.conversation == conversation,
    }
}

fn settled_snapshot() -> JobSnapshot {
    JobSnapshot {
        state: JobRunState::Settled,
        numbers: [0; 3],
        elapsed_ms: 0,
        stdout_bytes: 0,
        stderr_bytes: 0,
    }
}
