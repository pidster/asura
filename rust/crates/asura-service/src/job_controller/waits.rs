//! Parked waits (tool-job-controller.md, J3 exec_cmd as a job, the job
//! tool, and the J4 control handler): the exec_cmd yield window, the
//! model's `job wait`, and the client's parked `JobObserve` and `JobList`
//! with `after_revision`.
//!
//! No wait has a time bound of its own (no default time bounds). The yield
//! window's `yield_seconds` (WB1, from kind 26's commit, J3-E6) and the
//! model's optional `seconds` are caller-given bounds that end the wait,
//! never the job. Every other end is an event: kind 28 committed, stored
//! bytes past a value, a state change, a revision change, the caller's
//! cancel or detach, or shutdown. Waits are evaluated at the end of each
//! controller poll; ended waits queue for `drain_waits`. At most one agent
//! wait per agent (AE-03) and `CLIENT_WAITS` client waits exist.
//!
//! The conversation owner (J34-E, agent waits) and the job control handler
//! (J4-R, client waits) are the production callers; until they land only
//! the unit tests call these.
#![cfg_attr(not(test), allow(dead_code))]
use super::*;
use journal::{JobRunState, OutputStream};

/// The hard bound on parked client requests: `jobs.clientViewers` is at
/// most 32. The job control handler applies the configured value (default
/// 8) before it parks; this bound keeps the controller's map bounded alone.
pub(crate) const CLIENT_WAITS: usize = 32;

/// Who waits. An agent has one pending tool call (L9), so at most one
/// yield window or `job wait`. A client wait is keyed by the job control
/// handler's own request key (one pending request per attachment).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(crate) enum WaitKey {
    Agent(AgentKey),
    Client(u64),
}

/// Why a wait ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WaitEnding {
    /// The job's kind 28 committed while the wait was parked.
    Settled,
    /// `job wait` on a job whose kind 28 had committed at the call.
    AlreadySettled,
    /// A stream's stored bytes passed the value at the call (`job wait`)
    /// or the client's offset (`JobObserve`).
    NewOutput,
    /// `yield_seconds` passed since kind 26 committed, or the `wait`
    /// `seconds` passed since the call.
    Time,
    /// Yield window with `yield_seconds` 0: the job spawned, is queued with
    /// a block reason, or a cancel trigger reached it before spawn.
    Ready,
    /// `JobObserve`: the job's state differs from `after_state`.
    StateChanged,
    /// A parked `JobList`: the revision differs from `after_revision`.
    Revision(u64),
    /// The job left without a settlement record: a person job withdrawn
    /// before its kind 26 committed, or a job withdrawn or abandoned while
    /// the journal is unavailable. A model job ends this way only while the
    /// journal is unavailable (TJ-S1). The caller reads the journal or
    /// reports `unavailable`.
    Withdrawn,
    /// Shutdown: a parked client request is answered with `Draining`.
    Closed,
}

/// An ended wait. `job` is `None` for a listing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WaitDone {
    pub key: WaitKey,
    pub job: Option<Id>,
    pub end: WaitEnding,
}

/// The reply to a park call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WaitStart {
    /// Parked; the end arrives through `drain_waits`.
    Parked,
    /// Ended at once; nothing is parked.
    Done(WaitEnding),
    /// The controller does not hold or remember the job: it settled before
    /// this run or long ago, or does not exist. The caller reads the
    /// journal (`already_settled` or `not_found`).
    NotLive,
    /// The agent already has a wait, or `CLIENT_WAITS` client waits are
    /// parked.
    Busy,
}

/// What a turn end did to the agent's parked wait.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TurnWaitEnd {
    /// A yield window before handle delivery: the job was cancelled with
    /// requester model, reason `turn_ended` (TJ-04). The owner takes the
    /// view after the request for kind 29 with status 6.
    Yield { job: Id, reply: CancelReply },
    /// A `job wait`: the wait ended; the job continues (TJ-03).
    Wait { job: Id },
}

pub(super) enum Parked {
    Yield {
        job: Id,
        seconds: u32,
        /// Set at the first poll after kind 26 committed (J3-E6).
        until: Option<Instant>,
    },
    Wait {
        job: Id,
        base: [u64; 2],
        until: Option<Instant>,
    },
    Observe {
        job: Id,
        /// The named stream and the bytes the client holds; `None` with
        /// `at_end`: either stream past its stored bytes at the park.
        stream: Option<(usize, u64)>,
        base: [u64; 2],
        after: Option<u8>,
    },
    List {
        after: u64,
    },
}
impl Parked {
    fn job(&self) -> Option<Id> {
        match self {
            Self::Yield { job, .. } | Self::Wait { job, .. } | Self::Observe { job, .. } => {
                Some(*job)
            }
            Self::List { .. } => None,
        }
    }
    fn until(&self) -> Option<Instant> {
        match self {
            Self::Yield { until, .. } | Self::Wait { until, .. } => *until,
            _ => None,
        }
    }
}

impl JobController {
    /// The exec_cmd yield window (J3): the call's wait after `submit`
    /// accepted `job`. The window time starts when kind 26 commits
    /// (J3-E6); `seconds` is `yield_seconds`, 0 to 30. Ends: kind 28
    /// committed (`Settled`), the window time (`Time`), or with 0 the job
    /// spawned, queued with a block reason or cancelled before spawn
    /// (`Ready`). A revocation does not end the window; the job's kind 28
    /// or the window time does. The owner ends the window at a turn end
    /// with `end_turn_wait`.
    pub(crate) fn await_yield(&mut self, agent: AgentKey, job: Id, seconds: u32) -> WaitStart {
        if self.waits.contains_key(&WaitKey::Agent(agent)) {
            return WaitStart::Busy;
        }
        if !self.jobs.contains_key(&job) {
            return WaitStart::Done(if self.retired.iter().any(|r| r.job == job) {
                WaitEnding::Settled
            } else {
                WaitEnding::Withdrawn
            });
        }
        self.waits.insert(
            WaitKey::Agent(agent),
            Parked::Yield {
                job,
                seconds,
                until: None,
            },
        );
        WaitStart::Parked
    }

    /// The model's `job wait` (code 14): `AlreadySettled` at once when the
    /// job's kind 28 committed; otherwise parks until kind 28 commits,
    /// either stream's stored bytes pass their values now, or `seconds`
    /// (1 to 3,600) pass. With no `seconds` only events end it (TJ-30).
    pub(crate) fn await_job(
        &mut self,
        agent: AgentKey,
        job: Id,
        seconds: Option<u32>,
        now: Instant,
    ) -> WaitStart {
        if self.waits.contains_key(&WaitKey::Agent(agent)) {
            return WaitStart::Busy;
        }
        let base = match self.jobs.get(&job) {
            Some(j) if j.state == JobState::Recorded => {
                return WaitStart::Done(WaitEnding::AlreadySettled);
            }
            Some(j) => j.stored,
            None if self.retired.iter().any(|r| r.job == job) => {
                return WaitStart::Done(WaitEnding::AlreadySettled);
            }
            None => return WaitStart::NotLive,
        };
        let until = seconds.map(|s| now + Duration::from_secs(u64::from(s)));
        self.waits
            .insert(WaitKey::Agent(agent), Parked::Wait { job, base, until });
        WaitStart::Parked
    }

    /// A turn ended, was steered or invalidated, or the helper exited
    /// (J3): removes the agent's wait. A yield window cancels its job with
    /// requester model, reason `turn_ended`, because the model never
    /// received the handle (TJ-04); a `job wait` ends and the job
    /// continues (TJ-03). `None` when the agent had no wait.
    pub(crate) fn end_turn_wait(&mut self, agent: AgentKey) -> Option<TurnWaitEnd> {
        match self.waits.remove(&WaitKey::Agent(agent))? {
            Parked::Yield { job, .. } => {
                let reply = self.cancel(job, Requester::Model, JobReason::TurnEnded, false);
                Some(TurnWaitEnd::Yield { job, reply })
            }
            Parked::Wait { job, .. } => Some(TurnWaitEnd::Wait { job }),
            Parked::Observe { .. } | Parked::List { .. } => None,
        }
    }

    /// A client `JobObserve` (J4): ends at once, or parks until the named
    /// stream's stored bytes pass `offset` (either stream's present stored
    /// bytes with `at_end` and no stream), the job's state differs from
    /// `after_state`, or kind 28 commits. No hold time; detach removes it
    /// with `cancel_wait`. A second park under the same key replaces the
    /// first (one pending request per attachment).
    pub(crate) fn observe(
        &mut self,
        key: u64,
        job: Id,
        stream: Option<(OutputStream, u64)>,
        after_state: Option<JobRunState>,
    ) -> WaitStart {
        let Some(j) = self.jobs.get(&job) else {
            return if self.retired.iter().any(|r| r.job == job) {
                WaitStart::Done(WaitEnding::Settled)
            } else {
                WaitStart::NotLive
            };
        };
        let stream = stream.map(|(s, offset)| (stream_index(s), offset));
        let after = after_state.map(JobRunState::code);
        let parked = Parked::Observe {
            job,
            stream,
            base: j.stored,
            after,
        };
        if let Some(end) = self.observe_end(&parked) {
            return WaitStart::Done(end);
        }
        self.park_client(key, parked)
    }

    /// A client `JobList` with `after_revision` (J4): ends at once when
    /// the revision differs, else parks until it does. No hold time.
    pub(crate) fn park_list(&mut self, key: u64, after_revision: u64) -> WaitStart {
        if self.revision != after_revision {
            return WaitStart::Done(WaitEnding::Revision(self.revision));
        }
        self.park_client(
            key,
            Parked::List {
                after: after_revision,
            },
        )
    }

    /// Removes a parked wait without an end: a detached attachment, or a
    /// caller that no longer wants the reply. `false` when none was parked.
    pub(crate) fn cancel_wait(&mut self, key: WaitKey) -> bool {
        self.waits.remove(&key).is_some()
    }

    /// Ended waits, oldest first.
    pub(crate) fn drain_waits(&mut self, out: &mut Vec<WaitDone>) {
        out.extend(self.done.drain(..));
    }

    fn park_client(&mut self, key: u64, parked: Parked) -> WaitStart {
        let key = WaitKey::Client(key);
        let clients = self
            .waits
            .keys()
            .filter(|k| matches!(k, WaitKey::Client(_)))
            .count();
        if !self.waits.contains_key(&key) && clients >= CLIENT_WAITS {
            return WaitStart::Busy;
        }
        self.waits.insert(key, parked);
        WaitStart::Parked
    }

    /// Shutdown answers every parked client request (AE-06). Agent waits
    /// end with their turn or their job's settlement.
    pub(super) fn close_client_waits(&mut self) {
        let keys: Vec<WaitKey> = self
            .waits
            .keys()
            .filter(|k| matches!(k, WaitKey::Client(_)))
            .copied()
            .collect();
        for key in keys {
            if let Some(parked) = self.waits.remove(&key) {
                self.done.push_back(WaitDone {
                    key,
                    job: parked.job(),
                    end: WaitEnding::Closed,
                });
            }
        }
    }

    /// The earliest caller-given wait time, for the reactor's deadline.
    pub(super) fn wait_deadline(&self) -> Option<Instant> {
        self.waits.values().filter_map(Parked::until).min()
    }

    /// Ends every wait whose condition holds; starts each yield window
    /// whose kind 26 has committed.
    pub(super) fn check_waits(&mut self, now: Instant) {
        if self.waits.is_empty() {
            return;
        }
        let keys: Vec<WaitKey> = self.waits.keys().copied().collect();
        for key in keys {
            // The yield window starts at the first poll after kind 26
            // committed (J3-E6).
            let committed = |job: &Id| {
                self.jobs
                    .get(job)
                    .is_some_and(|j| j.intent == Intent::Committed)
            };
            let start = match self.waits.get(&key) {
                Some(Parked::Yield {
                    job,
                    seconds,
                    until: None,
                }) if committed(job) => Some(now + Duration::from_secs(u64::from(*seconds))),
                _ => None,
            };
            if let Some(at) = start
                && let Some(Parked::Yield { until, .. }) = self.waits.get_mut(&key)
            {
                *until = Some(at);
            }
            let end = match self.waits.get(&key) {
                Some(Parked::Yield {
                    job,
                    seconds,
                    until,
                }) => self.yield_end(*job, *seconds, *until, now),
                Some(Parked::Wait { job, base, until }) => self.settled_end(*job).or_else(|| {
                    let j = self.jobs.get(job)?;
                    if passed(j.stored, *base) {
                        Some(WaitEnding::NewOutput)
                    } else if until.is_some_and(|t| now >= t) {
                        Some(WaitEnding::Time)
                    } else {
                        None
                    }
                }),
                Some(parked @ Parked::Observe { .. }) => self.observe_end(parked),
                Some(Parked::List { after }) => {
                    (self.revision != *after).then_some(WaitEnding::Revision(self.revision))
                }
                None => None,
            };
            if let Some(end) = end
                && let Some(parked) = self.waits.remove(&key)
            {
                self.done.push_back(WaitDone {
                    key,
                    job: parked.job(),
                    end,
                });
            }
        }
    }

    fn yield_end(
        &self,
        job: Id,
        seconds: u32,
        until: Option<Instant>,
        now: Instant,
    ) -> Option<WaitEnding> {
        if let Some(end) = self.settled_end(job) {
            return Some(end);
        }
        let j = self.jobs.get(&job)?;
        // The window has not started before kind 26 commits.
        until?;
        if seconds == 0 {
            let blocked = j.state == JobState::Queued && j.block.is_some();
            let cancelled = j.cancel.is_some();
            return (j.spawned || blocked || cancelled).then_some(WaitEnding::Ready);
        }
        until.is_some_and(|t| now >= t).then_some(WaitEnding::Time)
    }

    fn observe_end(&self, parked: &Parked) -> Option<WaitEnding> {
        let Parked::Observe {
            job,
            stream,
            base,
            after,
        } = parked
        else {
            return None;
        };
        if let Some(end) = self.settled_end(*job) {
            return Some(end);
        }
        let j = self.jobs.get(job)?;
        if after.is_some_and(|code| code != self.run_state(j).code()) {
            return Some(WaitEnding::StateChanged);
        }
        let more = match *stream {
            Some((index, offset)) => j.stored[index] > offset,
            None => passed(j.stored, *base),
        };
        more.then_some(WaitEnding::NewOutput)
    }

    /// `Settled` when the job's kind 28 committed, `Withdrawn` when the
    /// controller no longer holds or remembers it.
    fn settled_end(&self, job: Id) -> Option<WaitEnding> {
        match self.jobs.get(&job) {
            Some(j) if j.state == JobState::Recorded => Some(WaitEnding::Settled),
            Some(j) if j.abandoned => Some(WaitEnding::Withdrawn),
            Some(_) => None,
            None if self.retired.iter().any(|r| r.job == job) => Some(WaitEnding::Settled),
            None => Some(WaitEnding::Withdrawn),
        }
    }
}

fn passed(now: [u64; 2], base: [u64; 2]) -> bool {
    now[0] > base[0] || now[1] > base[1]
}

fn stream_index(stream: OutputStream) -> usize {
    match stream {
        OutputStream::Stdout => 0,
        OutputStream::Stderr => 1,
    }
}
